# Source recipes

A recipe turns a generic transport (`websocket`, `sse`, `http-json`) plus an engine-side
**mapping** into a named source kind, so a new keyless JSON feed is a few lines of TOML and
no code — and it works identically in the native engine and the browser build, because the
mapping runs inside the shared engine.

```toml
[recipe]
kind = "kraken.trades"            # what a scene writes: source = { kind = "kraken.trades", symbols = ["BTC/USD"] }
summary = "Kraken spot trades (WebSocket v2, no key)"
network = true

[params.symbols]                  # user-facing parameters, substituted into {symbols} below
summary = "Pairs like BTC/USD"
default = ["BTC/USD"]

[spec]                            # the expanded SourceSpec; `{name}` placeholders are filled from params
kind = "websocket"
url = "wss://ws.kraken.com/v2"
subscribe = { method = "subscribe", params = { channel = "trade", symbol = "{symbols}" } }
filter = ".channel==trade"
each = ".data"
events_on = "every"

[spec.event]                      # engine-side mapping (see docs/ARCHITECTURE.md "Source mapping")
name = "trade"
intensity = "clamp(log10(1 + attr('price') * attr('qty')) / 5, 0, 1)"
attrs = { product = "symbol", price = "price", size = "qty", side = "side" }
derive = { usd = "attr('price') * attr('qty')", buy = "if(attr_text('side') == 'buy', 1, 0)" }
readings = { price = "attr('price')" }
```

Mapping keys: `name` (rename), `when` (expr; non-zero keeps), `intensity` (expr),
`attrs` (new name = raw attr key, passed through with its type), `derive` (new name = expr),
`readings` (channel = expr, pushed as a Reading alongside the event), `keep_raw` (bool, keep
the raw flattened attrs too; default false). Expressions are `on_event` expressions:
`attr("k")`, `attr_text("k") == "x"`, `attr_len("k")`, `has("k")`, `intensity`, `time`, `beat`.
