use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{broadcast, Mutex};

const RATE: f64 = 0.287_682_072_449_862; // ln(4/3)
const PRATE: f64 = RATE * 10.0; // pending rate
const SPY: f64 = 365.25 * 24.0 * 3600.0; // seconds per year
const TOTAL_SUPPLY: f64 = 9_223_372_036_854_775_807.0;
const GIFT: f64 = 100_000.0;

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

#[derive(Clone, Serialize)]
struct Wallet {
    name: String,
    deposits: f64,
    balance: f64,
    sent: f64,
    t: f64,
}

#[derive(Clone, Serialize)]
struct TxLog {
    from: String,
    to: String,
    amount: f64,
    t: f64,
}

#[derive(Serialize)]
struct Snapshot {
    wallets: Vec<Wallet>,
    log: Vec<TxLog>,
    rate: f64,
    prate: f64,
    spy: f64,
    t: f64,
}

struct App {
    wallets: Vec<Wallet>,
    log: Vec<TxLog>,
    notify: broadcast::Sender<()>,
}

impl App {
    fn new(notify: broadcast::Sender<()>) -> Self {
        let t = now();
        let mut wallets = vec![
            Wallet { name: "Koi".into(), deposits: 0.0, balance: TOTAL_SUPPLY, sent: 0.0, t },
            Wallet { name: "Alice".into(), deposits: 0.0, balance: 0.0, sent: 0.0, t },
            Wallet { name: "Bob".into(), deposits: 0.0, balance: 0.0, sent: 0.0, t },
            Wallet { name: "Carol".into(), deposits: 0.0, balance: 0.0, sent: 0.0, t },
            Wallet { name: "Dan".into(), deposits: 0.0, balance: 0.0, sent: 0.0, t },
            Wallet { name: "Eve".into(), deposits: 0.0, balance: 0.0, sent: 0.0, t },
        ];

        wallets[0].balance -= GIFT * 5.0;
        wallets[0].sent = GIFT * 5.0;
        let mut log = Vec::new();
        for i in 1..6 {
            wallets[i].deposits = GIFT;
            log.push(TxLog {
                from: "Koi".into(),
                to: wallets[i].name.clone(),
                amount: GIFT,
                t,
            });
        }

        App { wallets, log, notify }
    }

    fn settle(&mut self, i: usize) {
        let t = now();
        if i == 0 {
            return;
        }

        let w = &self.wallets[i];
        let dt = (t - w.t) / SPY;

        let pending = (w.deposits * ((PRATE * dt).exp() - 1.0)).min(w.deposits);
        let interest = (w.balance + pending) * ((RATE * dt).exp() - 1.0);

        self.wallets[i].balance += pending + interest;
        self.wallets[i].deposits -= pending;
        self.wallets[i].t = t;

        // Interest funded by Koi
        self.wallets[0].balance -= pending + interest;
    }

    fn early_settle(&mut self, i: usize, amount: f64) -> Result<(), String> {
        if i == 0 {
            return Err("Koi cannot settle".into());
        }
        let t = now();
        let w = &self.wallets[i];
        let dt = (t - w.t) / SPY;

        let pending = (w.deposits * ((PRATE * dt).exp() - 1.0)).min(w.deposits);
        let interest = (w.balance + pending) * ((RATE * dt).exp() - 1.0);
        let available = 3.0 * (w.deposits - pending) / 4.0;

        if amount > available {
            return Err("Exceeds available".into());
        }

        // Always settle pending + interest (free)
        self.wallets[i].balance += pending + interest;
        self.wallets[i].deposits -= pending;
        self.wallets[i].t = t;
        self.wallets[0].balance -= pending + interest;

        // Early settlement: wallet gets amount, fee goes to Koi deposits
        if amount > 0.0 {
            let fee = amount / 3.0;
            self.wallets[i].balance += amount;
            self.wallets[i].deposits -= amount + fee;
            self.wallets[0].balance += fee;
        }

        let name = self.wallets[i].name.clone();
        self.log.push(TxLog {
            from: name.clone(),
            to: name.clone(),
            amount,
            t,
        });
        if amount > 0.0 {
            self.log.push(TxLog {
                from: name,
                to: "Koi".into(),
                amount: amount / 3.0,
                t,
            });
        }

        let _ = self.notify.send(());
        Ok(())
    }

    fn send(&mut self, from: usize, to: usize, amount: f64) -> Result<(), String> {
        if amount <= 0.0 {
            return Err("Amount must be positive".into());
        }
        if from == to {
            return self.early_settle(from, amount);
        }

        self.settle(from);
        if self.wallets[from].balance < amount {
            return Err("Insufficient balance".into());
        }
        self.wallets[from].balance -= amount;
        self.wallets[from].sent += amount;

        self.settle(to);
        if to == 0 {
            // Koi receives directly to balance
            self.wallets[0].balance += amount;
        } else {
            self.wallets[to].deposits += amount;
        }

        let t = now();
        let from_name = self.wallets[from].name.clone();
        let to_name = self.wallets[to].name.clone();
        self.log.push(TxLog {
            from: from_name,
            to: to_name,
            amount,
            t,
        });

        let _ = self.notify.send(());
        Ok(())
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            wallets: self.wallets.clone(),
            log: self.log.clone(),
            rate: RATE,
            prate: PRATE,
            spy: SPY,
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
            Ok(()) => Json(SendRes { ok: true, error: None }),
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
