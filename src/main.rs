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
const SPY: f64 = 365.25 * 24.0 * 3600.0; // seconds per year
const TOTAL_SUPPLY: f64 = 1_000_000_000_000_000.0;
const GIFT: f64 = 1000.0;

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

#[derive(Clone, Serialize)]
struct Pending {
    amount: f64,
    t: f64,
    from: String,
}

#[derive(Clone, Serialize)]
struct Wallet {
    name: String,
    balance: f64,
    t: f64,
    pending: Vec<Pending>,
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
            Wallet { name: "Alice".into(), balance: TOTAL_SUPPLY, t, pending: vec![] },
            Wallet { name: "Bob".into(), balance: 0.0, t, pending: vec![] },
            Wallet { name: "Carol".into(), balance: 0.0, t, pending: vec![] },
            Wallet { name: "Dan".into(), balance: 0.0, t, pending: vec![] },
            Wallet { name: "Eve".into(), balance: 0.0, t, pending: vec![] },
        ];

        wallets[0].balance -= GIFT * 4.0;
        let mut log = Vec::new();
        for i in 1..5 {
            wallets[i].pending.push(Pending {
                amount: GIFT,
                t,
                from: "Alice".into(),
            });
            log.push(TxLog {
                from: "Alice".into(),
                to: wallets[i].name.clone(),
                amount: GIFT,
                t,
            });
        }

        App { wallets, log, notify }
    }

    fn settle(&mut self, i: usize) {
        let t = now();
        let w = &self.wallets[i];
        let dt = (t - w.t) / SPY;
        let base = w.balance * (RATE * dt).exp();

        let arrived: f64 = w
            .pending
            .iter()
            .map(|p| {
                let pdt = (t - p.t) / SPY;
                p.amount * (1.0 - (-RATE * pdt).exp())
            })
            .sum();

        let remaining: Vec<Pending> = w
            .pending
            .iter()
            .filter_map(|p| {
                let pdt = (t - p.t) / SPY;
                let r = p.amount * (-RATE * pdt).exp();
                (r > 1e-9).then(|| Pending {
                    amount: r,
                    t,
                    from: p.from.clone(),
                })
            })
            .collect();

        self.wallets[i].balance = base + arrived;
        self.wallets[i].t = t;
        self.wallets[i].pending = remaining;
    }

    fn send(&mut self, from: usize, to: usize, amount: f64) -> Result<(), String> {
        if from == to {
            return Err("Cannot send to self".into());
        }
        if amount <= 0.0 {
            return Err("Amount must be positive".into());
        }

        self.settle(from);
        if self.wallets[from].balance < amount {
            return Err("Insufficient balance".into());
        }
        self.wallets[from].balance -= amount;

        self.settle(to);
        let t = now();
        let from_name = self.wallets[from].name.clone();
        let to_name = self.wallets[to].name.clone();
        self.wallets[to].pending.push(Pending {
            amount,
            t,
            from: from_name.clone(),
        });
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
