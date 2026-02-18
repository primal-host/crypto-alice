use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use rand::distributions::WeightedIndex;
use rand::prelude::Distribution;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{broadcast, Mutex};

const RATE: f64 = 10.0 / 27.0; // ~37.04% base, 33.33% effective after 10% emission
const PRATE: f64 = RATE * 10.0; // vesting rate
const SPY: f64 = 365.25 * 24.0 * 3600.0; // seconds per year
const TOTAL_SUPPLY: f64 = 1_000_000_000.0;
const GIFT_ALICE: f64 = 10_000_000.0; // 1%
const GIFT_REST: f64 = 90_000_000.0;  // 9% divided randomly among remaining 997
const MILLIONAIRE_IDX: usize = 6;
const MILLIONAIRE_PAYOUT: f64 = 1_000_000.0;
const MILLIONAIRE_THRESHOLD: f64 = 1_001_001.0;

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

#[derive(Clone, Serialize)]
struct Wallet {
    name: String,
    contract: bool,
    locked: f64,
    vested: f64,
    balance: f64,
    sent: f64,
    t: f64,
}

#[derive(Clone, Serialize)]
struct TxLog {
    from: String,
    to: String,
    amount: f64,
    fee: f64,
    t: f64,
}

#[derive(Serialize)]
struct Snapshot {
    wallets: Vec<Wallet>,
    log: Vec<TxLog>,
    rate: f64,
    prate: f64,
    spy: f64,
    supply: f64,
    k0: f64,
    t: f64,
}

struct App {
    wallets: Vec<Wallet>,
    log: Vec<TxLog>,
    contributions: Vec<f64>,
    rng: StdRng,
    notify: broadcast::Sender<()>,
}

impl App {
    fn new(notify: broadcast::Sender<()>) -> Self {
        let t = now();
        let named = ["Koi", "Alice", "Bob", "Carol", "Dan", "Eve", "Millionaire"];
        let n = 100;
        let mut wallets = Vec::with_capacity(n);

        for i in 0..n {
            let name: String = if i < named.len() {
                named[i].into()
            } else {
                format!("W{:05}", i)
            };
            wallets.push(Wallet {
                name,
                contract: i == MILLIONAIRE_IDX,
                locked: 0.0,
                vested: 0.0,
                balance: 0.0,
                sent: 0.0,
                t,
            });
        }

        wallets[0].balance = TOTAL_SUPPLY;

        // Compute gift amounts (skip contracts)
        let mut gifts = vec![0.0; n];
        gifts[1] = GIFT_ALICE;
        let mut rng = rand::thread_rng();
        let weights: Vec<f64> = (2..n)
            .map(|i| if wallets[i].contract { 0.0 } else { rng.gen::<f64>() })
            .collect();
        let sum: f64 = weights.iter().sum();
        for (i, w) in weights.iter().enumerate() {
            gifts[i + 2] = GIFT_REST * w / sum;
        }

        let log = Vec::new();
        let contributions = vec![0.0; n];
        let rng = StdRng::from_entropy();
        let mut app = App { wallets, log, contributions, rng, notify };

        // Send gifts as real transactions (skip contracts)
        for i in 1..n {
            if app.wallets[i].contract {
                continue;
            }
            let _ = app.send(0, i, gifts[i]);
        }

        app
    }

    fn settle(&mut self, i: usize) {
        let t = now();
        if i == 0 || self.wallets[i].contract {
            return;
        }

        let w = &self.wallets[i];
        let dt = (t - w.t) / SPY;

        let vested = (w.locked * ((PRATE * dt).exp() - 1.0)).min(w.locked);
        let erate = RATE * self.wallets[0].balance.max(0.0) / TOTAL_SUPPLY;
        let interest = (w.balance + w.vested + vested) * ((erate * dt).exp() - 1.0);

        self.wallets[i].balance += interest;
        self.wallets[i].vested += vested;
        self.wallets[i].locked -= vested;
        self.wallets[i].t = t;

        // Only interest funded by Koi
        self.wallets[0].balance -= interest;
    }

    fn early_settle(&mut self, i: usize, amount: f64) -> Result<(), String> {
        if i == 0 {
            return Err("Koi cannot settle".into());
        }
        if amount < 0.0 {
            return Err("Amount must be non-negative".into());
        }

        self.settle(i);

        if amount > 0.0 {
            let available = 3.0 * self.wallets[i].locked / 4.0;
            if amount > available {
                return Err("Exceeds available".into());
            }
        }

        // Move all vested to balance (free)
        let claimed = self.wallets[i].vested;
        self.wallets[i].balance += claimed;
        self.wallets[i].vested = 0.0;

        // Early settlement: wallet gets amount, fee goes to Koi
        if amount > 0.0 {
            let fee = amount / 3.0;
            self.wallets[i].balance += amount;
            self.wallets[i].locked -= amount + fee;
            self.wallets[0].balance += fee;
        }

        let t = now();
        let name = self.wallets[i].name.clone();
        let total = claimed + amount;
        if total > 0.0 {
            self.log.push(TxLog {
                from: name.clone(),
                to: name.clone(),
                amount: total,
                fee: if amount > 0.0 { amount / 3.0 } else { 0.0 },
                t,
            });
        }

        let _ = self.notify.send(());
        Ok(())
    }

    fn send(&mut self, from: usize, to: usize, amount: f64) -> Result<(), String> {
        if from == to {
            return self.early_settle(from, amount);
        }
        if amount <= 0.0 {
            return Err("Amount must be positive".into());
        }

        self.settle(from);

        if self.wallets[from].balance < amount {
            return Err("Insufficient balance".into());
        }
        self.wallets[from].balance -= amount;
        self.wallets[from].sent += amount;

        // 0.1% fee on wallet-to-wallet (not involving Koi)
        let mut send_amount = amount;
        let fee = if from != 0 && to != 0 {
            let fee = amount / 1000.0;
            let mut rem = fee;

            // Source fee: locked first
            let fl = rem.min(self.wallets[from].locked);
            self.wallets[from].locked -= fl;
            rem -= fl;

            // Then vested
            let fv = rem.min(self.wallets[from].vested);
            self.wallets[from].vested -= fv;
            rem -= fv;

            // Then excess balance (already deducted amount)
            let fb = rem.min(self.wallets[from].balance);
            self.wallets[from].balance -= fb;
            rem -= fb;

            // Remainder reduces the transfer amount
            send_amount -= rem;

            self.wallets[0].balance += fee;
            fee
        } else {
            0.0
        };

        if to == MILLIONAIRE_IDX {
            self.contributions[from] += send_amount;
        }

        self.settle(to);
        if to == 0 || self.wallets[to].contract {
            self.wallets[to].balance += send_amount;
        } else {
            self.wallets[to].balance += send_amount / 3.0;
            self.wallets[to].locked += 2.0 * send_amount / 3.0;
        }

        let t = now();
        let from_name = self.wallets[from].name.clone();
        let to_name = self.wallets[to].name.clone();
        self.log.push(TxLog {
            from: from_name,
            to: to_name,
            amount: send_amount,
            fee,
            t,
        });

        let _ = self.notify.send(());
        Ok(())
    }

    fn check_millionaire(&mut self) {
        if self.wallets[MILLIONAIRE_IDX].balance <= MILLIONAIRE_THRESHOLD {
            return;
        }
        // Build weighted distribution from contributions
        let weights: Vec<(usize, f64)> = self
            .contributions
            .iter()
            .enumerate()
            .filter(|(_, &c)| c > 0.0)
            .map(|(i, &c)| (i, c))
            .collect();
        if weights.is_empty() {
            return;
        }
        let dist = WeightedIndex::new(weights.iter().map(|&(_, w)| w)).unwrap();
        let winner = weights[dist.sample(&mut self.rng)].0;
        let _ = self.send(MILLIONAIRE_IDX, winner, MILLIONAIRE_PAYOUT);
        // Reset contributions
        for c in self.contributions.iter_mut() {
            *c = 0.0;
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            wallets: self.wallets.clone(),
            log: self.log.clone(),
            rate: RATE,
            prate: PRATE,
            spy: SPY,
            supply: TOTAL_SUPPLY,
            k0: TOTAL_SUPPLY - GIFT_ALICE - GIFT_REST,
            t: now(),
        }
    }
}

type S = Arc<Mutex<App>>;

async fn index() -> Html<&'static str> {
    Html(include_str!("index.html"))
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(s): State<S>) -> impl IntoResponse {
    ws.on_upgrade(|sock| ws_handler(sock, s))
}

async fn ws_handler(mut sock: WebSocket, state: S) {
    let snap = state.lock().await.snapshot();
    if let Ok(msg) = serde_json::to_string(&snap) {
        if sock.send(Message::Text(msg)).await.is_err() {
            return;
        }
    }

    let mut rx = state.lock().await.notify.subscribe();

    loop {
        tokio::select! {
            r = rx.recv() => {
                if r.is_err() { break; }
                let snap = state.lock().await.snapshot();
                if let Ok(msg) = serde_json::to_string(&snap) {
                    if sock.send(Message::Text(msg)).await.is_err() { break; }
                }
            }
            msg = sock.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }
}

#[derive(Deserialize)]
struct SendReq {
    from: String,
    to: String,
    amount: f64,
}

#[derive(Serialize)]
struct SendRes {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn send_handler(State(s): State<S>, Json(req): Json<SendReq>) -> Json<SendRes> {
    let mut app = s.lock().await;
    let fi = app.wallets.iter().position(|w| w.name == req.from);
    let ti = app.wallets.iter().position(|w| w.name == req.to);
    match (fi, ti) {
        (Some(f), Some(t)) => match app.send(f, t, req.amount) {
            Ok(()) => {
                app.check_millionaire();
                Json(SendRes { ok: true, error: None })
            }
            Err(e) => Json(SendRes { ok: false, error: Some(e) }),
        },
        _ => Json(SendRes {
            ok: false,
            error: Some("Unknown wallet".into()),
        }),
    }
}

#[tokio::main]
async fn main() {
    let (tx, _) = broadcast::channel(64);
    let state: S = Arc::new(Mutex::new(App::new(tx)));

    // Random transactions once per second
    let sim = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            interval.tick().await;
            let mut app = sim.lock().await;
            let n = app.wallets.len();
            // Pick from excluding Koi (0), Alice (1), and Millionaire (6)
            let mut from = app.rng.gen_range(2..n - 1); // n-1 candidates (skip one)
            if from >= MILLIONAIRE_IDX {
                from += 1; // skip over Millionaire
            }
            // 50% chance target is Millionaire, otherwise random wallet
            let to = if app.rng.gen_bool(0.5) {
                MILLIONAIRE_IDX
            } else {
                let mut t = app.rng.gen_range(2..n - 2); // exclude Millionaire and from
                if t >= MILLIONAIRE_IDX.min(from) {
                    t += 1;
                }
                if t >= MILLIONAIRE_IDX.max(from) {
                    t += 1;
                }
                t
            };
            let pct = app.rng.gen_range(0.001..=0.01);
            let bal = app.wallets[from].balance;
            if bal > 0.0 {
                let amount = bal * pct;
                let _ = app.send(from, to, amount);
                app.check_millionaire();
            }
        }
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/ws", get(ws_upgrade))
        .route("/api/send", post(send_handler))
        .with_state(state);

    let addr = "0.0.0.0:3000";
    println!("listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
