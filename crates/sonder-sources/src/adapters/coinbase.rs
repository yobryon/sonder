//! `coinbase.trades` — live matches from the Coinbase Exchange WebSocket feed.

use std::collections::VecDeque;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value as Json};
use sonder_core::{Seconds, SourceSpec, SourceState};
use tokio_tungstenite::tungstenite::Message;

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, clamp01, jf, js, Attrs, Emitter, RateWindow};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

pub const URL: &str = "wss://ws-feed.exchange.coinbase.com";

/// A parsed `match` message.
#[derive(Debug, Clone, PartialEq)]
pub struct Trade {
    pub product: String,
    pub side: String,
    pub price: f64,
    pub size: f64,
    pub usd: f64,
}

impl Trade {
    pub fn intensity(&self) -> f64 {
        clamp01((1.0 + self.usd).log10() / 5.0)
    }
    pub fn attrs(&self) -> Attrs {
        Attrs::new()
            .text("product", self.product.clone())
            .text("side", self.side.clone())
            .num("price", self.price)
            .num("size", self.size)
            .num("usd", self.usd)
            .bool("buy", self.side == "buy")
    }
}

/// Parse a Coinbase `match`/`last_match` message; other message types → None.
pub fn parse_match(v: &Json) -> Option<Trade> {
    let ty = js(v, "type")?;
    if ty != "match" && ty != "last_match" {
        return None;
    }
    let price = jf(v, "price")?;
    let size = jf(v, "size")?;
    Some(Trade {
        product: js(v, "product_id").unwrap_or("").to_string(),
        side: js(v, "side").unwrap_or("").to_string(),
        price,
        size,
        usd: price * size,
    })
}

/// Rolling stats for the 1 Hz readings.
#[derive(Debug, Default)]
pub struct TradeStats {
    pub last_price: Option<f64>,
    prices: VecDeque<(Seconds, f64)>,
    trades: RateWindow,
    buys: RateWindow,
    volume: VecDeque<(Seconds, f64)>,
}

impl TradeStats {
    pub fn push(&mut self, at: Seconds, t: &Trade) {
        self.last_price = Some(t.price);
        self.prices.push_back((at, t.price));
        self.trades.push(at);
        if t.side == "buy" {
            self.buys.push(at);
        }
        self.volume.push_back((at, t.usd));
        while self.prices.len() > 50_000 {
            self.prices.pop_front();
        }
        while self.volume.len() > 50_000 {
            self.volume.pop_front();
        }
    }

    pub fn readings(&mut self, now: Seconds) -> Vec<(&'static str, f64)> {
        while let Some(&(t, _)) = self.prices.front() {
            if now - t > 60.0 {
                self.prices.pop_front();
            } else {
                break;
            }
        }
        while let Some(&(t, _)) = self.volume.front() {
            if now - t > 10.0 {
                self.volume.pop_front();
            } else {
                break;
            }
        }
        let price = self.last_price.unwrap_or(0.0);
        let oldest = self.prices.front().map(|(_, p)| *p).unwrap_or(price);
        let change = if oldest > 0.0 { (price - oldest) / oldest * 100.0 } else { 0.0 };
        let n = self.trades.count(now, 10.0) as f64;
        let b = self.buys.count(now, 10.0) as f64;
        let rate = n / 10.0;
        let buy_ratio = if n > 0.0 { b / n } else { 0.5 };
        let usd_per_s = self.volume.iter().map(|(_, u)| u).sum::<f64>() / 10.0;
        vec![("price", price), ("price_change", change), ("rate", rate), ("buy_ratio", buy_ratio), ("usd_rate", usd_per_s)]
    }
}

struct CoinbaseSource {
    products: Vec<String>,
}

impl Source for CoinbaseSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let products = self.products;
            let mut em = Emitter::new(ctx);
            let mut stats = TradeStats::default();
            let mut rng = util::rng_from_seed(util::instance_seed(em.ctx.seed, em.ctx.id.as_str(), None));
            let mut attempt = 0u32;
            loop {
                if em.is_shutdown() {
                    return;
                }
                em.status_force(SourceState::Connecting, "connecting").await;
                let started = std::time::Instant::now();
                let res = session(&mut em, &products, &mut stats).await;
                if em.is_shutdown() {
                    return;
                }
                if started.elapsed() > Duration::from_secs(60) {
                    attempt = 0;
                }
                let d = util::retry_delay(attempt, &mut rng);
                attempt = attempt.saturating_add(1);
                let msg = res.err().unwrap_or_else(|| "connection closed".into());
                em.status_force(SourceState::Reconnecting, format!("{msg}; reconnect in {:.0}s", d.as_secs_f64())).await;
                if !em.sleep(d).await {
                    return;
                }
            }
        })
    }
}

/// Build the subscribe message.
pub fn subscribe_msg(products: &[String]) -> Json {
    json!({"type": "subscribe", "product_ids": products, "channels": ["matches", "heartbeat"]})
}

async fn session(em: &mut Emitter, products: &[String], stats: &mut TradeStats) -> Result<(), String> {
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(20), tokio_tungstenite::connect_async(URL))
        .await
        .map_err(|_| "connect timeout".to_string())?
        .map_err(|e| format!("connect failed: {e}"))?;
    ws.send(Message::text(subscribe_msg(products).to_string())).await.map_err(|e| format!("subscribe failed: {e}"))?;
    let mut next_reading = em.now() + 1.0;
    let mut last_msg = std::time::Instant::now();
    loop {
        if em.is_shutdown() {
            let _ = ws.close(None).await;
            return Ok(());
        }
        let msg = tokio::select! {
            m = ws.next() => m,
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                if last_msg.elapsed() > Duration::from_secs(60) {
                    return Err("no messages for 60 s".into());
                }
                None
            }
        };
        match msg {
            Some(Ok(Message::Text(txt))) => {
                last_msg = std::time::Instant::now();
                if let Ok(v) = serde_json::from_str::<Json>(txt.as_str()) {
                    match js(&v, "type") {
                        Some("error") => return Err(format!("feed error: {}", js(&v, "message").unwrap_or("?"))),
                        Some("subscriptions") => em.status(SourceState::Live, "subscribed").await,
                        _ => {
                            if let Some(t) = parse_match(&v) {
                                stats.push(em.now(), &t);
                                em.status(SourceState::Live, "live").await;
                                em.event("trade", t.intensity(), t.attrs().into_map()).await;
                            }
                        }
                    }
                }
            }
            Some(Ok(Message::Ping(p))) => {
                let _ = ws.send(Message::Pong(p)).await;
            }
            Some(Ok(Message::Close(_))) => return Err("server closed".into()),
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(format!("ws error: {e}")),
            None => {}
        }
        let now = em.now();
        if now >= next_reading {
            next_reading = now + 1.0;
            if stats.last_price.is_some() {
                em.reading(stats.readings(now)).await;
            }
        }
    }
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "coinbase.trades",
        summary: "Live trades from Coinbase Exchange (WebSocket): a `trade` event per match",
        params: vec![ParamInfo { name: "products", summary: "Product ids, e.g. [\"BTC-USD\", \"ETH-USD\"]", default: Some("[\"BTC-USD\"]") }],
        channels: vec!["price", "price_change", "rate", "buy_ratio", "usd_rate"],
        events: vec!["trade"],
        network: true,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    let products = util::param_str_list(spec, "products")
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| vec!["BTC-USD".to_string()])
        .into_iter()
        .map(|p| p.to_ascii_uppercase())
        .collect::<Vec<_>>();
    for p in &products {
        if !p.contains('-') {
            return Err(util::invalid(spec, "products", &format!("`{p}` is not a product id like BTC-USD")));
        }
    }
    Ok(Box::new(CoinbaseSource { products }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_match_and_stats() {
        let v: Json = serde_json::from_str(r#"{"type":"match","trade_id":12345,"maker_order_id":"a","taker_order_id":"b","side":"sell","size":"0.025","price":"64321.5","product_id":"BTC-USD","sequence":1,"time":"2024-01-01T00:00:00.000000Z"}"#).unwrap();
        let t = parse_match(&v).unwrap();
        assert_eq!(t.product, "BTC-USD");
        assert!((t.usd - 1608.0375).abs() < 1e-6);
        assert!((t.intensity() - (1609.0375f64.log10() / 5.0)).abs() < 1e-9);
        assert!(parse_match(&json!({"type": "heartbeat"})).is_none());

        let mut s = TradeStats::default();
        s.push(0.0, &Trade { product: "BTC-USD".into(), side: "buy".into(), price: 100.0, size: 1.0, usd: 100.0 });
        s.push(5.0, &Trade { product: "BTC-USD".into(), side: "sell".into(), price: 110.0, size: 1.0, usd: 110.0 });
        let r: std::collections::BTreeMap<_, _> = s.readings(6.0).into_iter().collect();
        assert_eq!(r["price"], 110.0);
        assert!((r["price_change"] - 10.0).abs() < 1e-9);
        assert_eq!(r["buy_ratio"], 0.5);
        assert!((r["rate"] - 0.2).abs() < 1e-9);
    }

    #[test]
    fn subscribe_shape() {
        let m = subscribe_msg(&["BTC-USD".into()]);
        assert_eq!(m["type"], "subscribe");
        assert_eq!(m["product_ids"][0], "BTC-USD");
        assert!(factory(&SourceSpec::new("coinbase.trades").with("products", "btc")).is_err());
    }

    /// `cargo test -p sonder-sources -- --ignored coinbase`
    #[tokio::test]
    #[ignore]
    async fn live_matches_parse() {
        let (mut ws, _) = tokio_tungstenite::connect_async(URL).await.unwrap();
        ws.send(Message::text(subscribe_msg(&["BTC-USD".into()]).to_string())).await.unwrap();
        let mut trades = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while trades < 3 && std::time::Instant::now() < deadline {
            let Ok(Some(Ok(Message::Text(t)))) = tokio::time::timeout(Duration::from_secs(10), ws.next()).await else { break };
            if let Ok(v) = serde_json::from_str::<Json>(t.as_str()) {
                if parse_match(&v).is_some() {
                    trades += 1;
                }
            }
        }
        assert!(trades >= 1);
    }
}
