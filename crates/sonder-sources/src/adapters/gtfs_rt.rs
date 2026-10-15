//! `transit.gtfs-rt` — GTFS-Realtime protobuf feeds (vehicle positions, trip updates,
//! service alerts), polled over HTTP.
//!
//! The protobuf schema is a hand-written `prost` subset of the official
//! `gtfs-realtime.proto` (tag numbers verified against the upstream file; unknown fields
//! are skipped by prost), so no build script or `protoc` is needed.
//!
//! Feeds come in two shapes: one URL per entity type (MBTA, Metro Transit, most agencies)
//! or a single combined feed (MTA NYC subway). `url` is the primary feed; optional
//! `trip_updates_url` / `alerts_url` are merged into the same poll. Events found in a poll
//! are emitted *spread over the interval* rather than all at once, so a poll that reveals
//! 200 arrivals becomes a steady patter instead of a single thud.
//!
//! Verified live (2026-10): MBTA (`cdn.mbta.com/realtime/{VehiclePositions,TripUpdates,
//! Alerts}.pb`) and Metro Transit MN (`svc.metrotransit.org/mtgtfs/vehiclepositions.pb`)
//! need no key; the MTA NYC subway feed
//! `https://api-endpoint.mta.info/Dataservice/mtagtfsfeeds/nyct%2Fgtfs` answers 200 with no
//! `x-api-key` header (the former key requirement was dropped), served as `text/plain`.
//! Delay fields: MBTA and MTA publish predicted stop times only, so `mean_delay`/`late`
//! stay 0 for them (no static schedule here to diff against); Metro Transit fills `delay`.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use prost::Message;
use serde_json::Value as Json;
use sonder_core::SourceSpec;

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, clamp01, Attrs, Emitter, Poll, SeenSet};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

pub const DEFAULT_URL: &str = "https://cdn.mbta.com/realtime/VehiclePositions.pb";
pub const DEFAULT_TRIP_UPDATES_URL: &str = "https://cdn.mbta.com/realtime/TripUpdates.pb";
pub const DEFAULT_ALERTS_URL: &str = "https://cdn.mbta.com/realtime/Alerts.pb";
/// Other keyless feeds that decode with this adapter.
pub const MTA_SUBWAY_URL: &str = "https://api-endpoint.mta.info/Dataservice/mtagtfsfeeds/nyct%2Fgtfs";
pub const METRO_TRANSIT_URL: &str = "https://svc.metrotransit.org/mtgtfs/vehiclepositions.pb";
/// Most events emitted per poll (the rest are dropped, oldest stop changes first kept).
const MAX_EVENTS_PER_POLL: usize = 300;

// ---------------------------------------------------------------------------
// Protobuf subset (gtfs-realtime.proto, proto2)
// ---------------------------------------------------------------------------

pub mod proto {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct FeedMessage {
        #[prost(message, optional, tag = "1")]
        pub header: Option<FeedHeader>,
        #[prost(message, repeated, tag = "2")]
        pub entity: Vec<FeedEntity>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct FeedHeader {
        #[prost(string, optional, tag = "1")]
        pub gtfs_realtime_version: Option<String>,
        #[prost(int32, optional, tag = "2")]
        pub incrementality: Option<i32>,
        #[prost(uint64, optional, tag = "3")]
        pub timestamp: Option<u64>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct FeedEntity {
        #[prost(string, tag = "1")]
        pub id: String,
        #[prost(bool, optional, tag = "2")]
        pub is_deleted: Option<bool>,
        #[prost(message, optional, tag = "3")]
        pub trip_update: Option<TripUpdate>,
        #[prost(message, optional, tag = "4")]
        pub vehicle: Option<VehiclePosition>,
        #[prost(message, optional, tag = "5")]
        pub alert: Option<Alert>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct TripUpdate {
        #[prost(message, optional, tag = "1")]
        pub trip: Option<TripDescriptor>,
        #[prost(message, optional, tag = "3")]
        pub vehicle: Option<VehicleDescriptor>,
        #[prost(message, repeated, tag = "2")]
        pub stop_time_update: Vec<StopTimeUpdate>,
        #[prost(uint64, optional, tag = "4")]
        pub timestamp: Option<u64>,
        #[prost(int32, optional, tag = "5")]
        pub delay: Option<i32>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct StopTimeEvent {
        #[prost(int32, optional, tag = "1")]
        pub delay: Option<i32>,
        #[prost(int64, optional, tag = "2")]
        pub time: Option<i64>,
        #[prost(int32, optional, tag = "3")]
        pub uncertainty: Option<i32>,
        #[prost(int64, optional, tag = "4")]
        pub scheduled_time: Option<i64>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct StopTimeUpdate {
        #[prost(uint32, optional, tag = "1")]
        pub stop_sequence: Option<u32>,
        #[prost(string, optional, tag = "4")]
        pub stop_id: Option<String>,
        #[prost(message, optional, tag = "2")]
        pub arrival: Option<StopTimeEvent>,
        #[prost(message, optional, tag = "3")]
        pub departure: Option<StopTimeEvent>,
        #[prost(int32, optional, tag = "5")]
        pub schedule_relationship: Option<i32>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct VehiclePosition {
        #[prost(message, optional, tag = "1")]
        pub trip: Option<TripDescriptor>,
        #[prost(message, optional, tag = "8")]
        pub vehicle: Option<VehicleDescriptor>,
        #[prost(message, optional, tag = "2")]
        pub position: Option<Position>,
        #[prost(uint32, optional, tag = "3")]
        pub current_stop_sequence: Option<u32>,
        #[prost(string, optional, tag = "7")]
        pub stop_id: Option<String>,
        /// `VehicleStopStatus`: 0 INCOMING_AT, 1 STOPPED_AT, 2 IN_TRANSIT_TO (default).
        #[prost(int32, optional, tag = "4")]
        pub current_status: Option<i32>,
        #[prost(uint64, optional, tag = "5")]
        pub timestamp: Option<u64>,
        #[prost(int32, optional, tag = "6")]
        pub congestion_level: Option<i32>,
        /// `OccupancyStatus`: 0 EMPTY … 5 FULL, 6 NOT_ACCEPTING_PASSENGERS, 7 NO_DATA, 8 NOT_BOARDABLE.
        #[prost(int32, optional, tag = "9")]
        pub occupancy_status: Option<i32>,
        #[prost(uint32, optional, tag = "10")]
        pub occupancy_percentage: Option<u32>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Position {
        #[prost(float, tag = "1")]
        pub latitude: f32,
        #[prost(float, tag = "2")]
        pub longitude: f32,
        #[prost(float, optional, tag = "3")]
        pub bearing: Option<f32>,
        #[prost(double, optional, tag = "4")]
        pub odometer: Option<f64>,
        /// Metres per second.
        #[prost(float, optional, tag = "5")]
        pub speed: Option<f32>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct TripDescriptor {
        #[prost(string, optional, tag = "1")]
        pub trip_id: Option<String>,
        #[prost(string, optional, tag = "5")]
        pub route_id: Option<String>,
        #[prost(uint32, optional, tag = "6")]
        pub direction_id: Option<u32>,
        #[prost(string, optional, tag = "2")]
        pub start_time: Option<String>,
        #[prost(string, optional, tag = "3")]
        pub start_date: Option<String>,
        #[prost(int32, optional, tag = "4")]
        pub schedule_relationship: Option<i32>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct VehicleDescriptor {
        #[prost(string, optional, tag = "1")]
        pub id: Option<String>,
        #[prost(string, optional, tag = "2")]
        pub label: Option<String>,
        #[prost(string, optional, tag = "3")]
        pub license_plate: Option<String>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Alert {
        #[prost(message, repeated, tag = "5")]
        pub informed_entity: Vec<EntitySelector>,
        /// `Cause`: 1 UNKNOWN_CAUSE … 13 SPECIAL_EVENT.
        #[prost(int32, optional, tag = "6")]
        pub cause: Option<i32>,
        /// `Effect`: 1 NO_SERVICE … 11 ACCESSIBILITY_ISSUE.
        #[prost(int32, optional, tag = "7")]
        pub effect: Option<i32>,
        #[prost(message, optional, tag = "10")]
        pub header_text: Option<TranslatedString>,
        #[prost(int32, optional, tag = "14")]
        pub severity_level: Option<i32>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct EntitySelector {
        #[prost(string, optional, tag = "1")]
        pub agency_id: Option<String>,
        #[prost(string, optional, tag = "2")]
        pub route_id: Option<String>,
        #[prost(int32, optional, tag = "3")]
        pub route_type: Option<i32>,
        #[prost(message, optional, tag = "4")]
        pub trip: Option<TripDescriptor>,
        #[prost(string, optional, tag = "5")]
        pub stop_id: Option<String>,
        #[prost(uint32, optional, tag = "6")]
        pub direction_id: Option<u32>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct TranslatedString {
        #[prost(message, repeated, tag = "1")]
        pub translation: Vec<Translation>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Translation {
        #[prost(string, tag = "1")]
        pub text: String,
        #[prost(string, optional, tag = "2")]
        pub language: Option<String>,
    }
}

pub const STATUS_IN_TRANSIT_TO: i32 = 2;
pub const STATUS_STOPPED_AT: i32 = 1;

pub fn status_name(s: i32) -> &'static str {
    match s {
        0 => "incoming_at",
        1 => "stopped_at",
        2 => "in_transit_to",
        _ => "unknown",
    }
}

pub fn cause_name(c: i32) -> &'static str {
    match c {
        1 => "unknown", 2 => "other", 3 => "technical_problem", 4 => "strike", 5 => "demonstration",
        6 => "accident", 7 => "holiday", 8 => "weather", 9 => "maintenance", 10 => "construction",
        11 => "police_activity", 12 => "medical_emergency", 13 => "special_event", _ => "unknown",
    }
}

pub fn effect_name(e: i32) -> &'static str {
    match e {
        1 => "no_service", 2 => "reduced_service", 3 => "significant_delays", 4 => "detour",
        5 => "additional_service", 6 => "modified_service", 7 => "other", 8 => "unknown",
        9 => "stop_moved", 10 => "no_effect", 11 => "accessibility_issue", _ => "unknown",
    }
}

/// How loud an alert is, by effect.
pub fn effect_intensity(e: i32) -> f64 {
    match e {
        1 => 1.0,
        3 => 0.8,
        2 => 0.7,
        4 | 6 => 0.6,
        9 | 11 => 0.5,
        _ => 0.4,
    }
}

/// Decode a feed body; HTML error pages get a clearer message than prost's.
pub fn decode_feed(body: &[u8]) -> Result<proto::FeedMessage, String> {
    // A FeedMessage always starts with the header tag (0x0a); HTML/JSON bodies start with
    // `<` or `{`. (Do not trim whitespace first: 0x0a is also `\n`.)
    if body.first().is_some_and(|b| *b == b'<' || *b == b'{') {
        return Err("response is not a protobuf (HTML/JSON body; wrong URL or missing API key?)".into());
    }
    proto::FeedMessage::decode(body).map_err(|e| format!("protobuf decode failed: {e}"))
}

// ---------------------------------------------------------------------------
// Flattened snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Vehicle {
    pub id: String,
    pub label: Option<String>,
    pub route: Option<String>,
    pub trip_id: Option<String>,
    pub direction: Option<u32>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub bearing: Option<f64>,
    /// m/s
    pub speed: Option<f64>,
    pub status: Option<i32>,
    pub stop_seq: Option<u32>,
    pub stop_id: Option<String>,
    pub occupancy: Option<i32>,
    pub timestamp: Option<u64>,
}

impl Vehicle {
    /// Moving: a positive speed, or (without speed) an explicit IN_TRANSIT_TO status.
    pub fn moving(&self) -> bool {
        match self.speed {
            Some(s) => s > 0.3,
            None => self.status == Some(STATUS_IN_TRANSIT_TO),
        }
    }
    pub fn stopped(&self) -> bool {
        self.status == Some(STATUS_STOPPED_AT) || self.speed.is_some_and(|s| s <= 0.3)
    }
    /// `(stop_seq, stop_id)` — what the `arrival` detector compares between polls.
    pub fn stop_key(&self) -> Option<(Option<u32>, Option<String>)> {
        if self.stop_seq.is_none() && self.stop_id.is_none() {
            None
        } else {
            Some((self.stop_seq, self.stop_id.clone()))
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TripDelay {
    pub trip_id: String,
    pub route: Option<String>,
    pub delay_s: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlertInfo {
    pub id: String,
    pub cause: i32,
    pub effect: i32,
    pub routes: Vec<String>,
    pub informed: usize,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub vehicles: Vec<Vehicle>,
    pub delays: Vec<TripDelay>,
    pub alerts: Vec<AlertInfo>,
    /// Newest header timestamp among the feeds.
    pub feed_ts: Option<u64>,
    pub entities: usize,
    /// Trip updates seen (whether or not they carried a delay).
    pub trip_updates: usize,
}

/// Delay (s) of a trip update: the top-level `delay`, else the first stop-time event with a
/// `delay`, else `time − scheduled_time` where both are given. Agencies that publish only
/// predicted times (MBTA, MTA NYC) yield `None`; Metro Transit MN fills `delay`.
pub fn trip_delay(tu: &proto::TripUpdate) -> Option<f64> {
    if let Some(d) = tu.delay {
        return Some(f64::from(d));
    }
    let event_delay = |e: &proto::StopTimeEvent| {
        e.delay.map(f64::from).or_else(|| match (e.time, e.scheduled_time) {
            (Some(t), Some(s)) => Some((t - s) as f64),
            _ => None,
        })
    };
    tu.stop_time_update
        .iter()
        .find_map(|s| s.arrival.as_ref().and_then(event_delay).or_else(|| s.departure.as_ref().and_then(event_delay)))
}

fn route_ok(routes: Option<&HashSet<String>>, route: Option<&str>) -> bool {
    match routes {
        None => true,
        Some(set) => route.is_some_and(|r| set.contains(r)),
    }
}

/// Flatten one or more feeds into vehicles, trip delays and alerts, applying the route filter.
pub fn summarize(feeds: &[proto::FeedMessage], routes: Option<&HashSet<String>>) -> Snapshot {
    let mut snap = Snapshot::default();
    for feed in feeds {
        if let Some(ts) = feed.header.as_ref().and_then(|h| h.timestamp) {
            snap.feed_ts = Some(snap.feed_ts.map_or(ts, |t| t.max(ts)));
        }
        snap.entities += feed.entity.len();
        for ent in &feed.entity {
            if ent.is_deleted == Some(true) {
                continue;
            }
            if let Some(v) = &ent.vehicle {
                let route = v.trip.as_ref().and_then(|t| t.route_id.clone());
                if !route_ok(routes, route.as_deref()) {
                    continue;
                }
                let id = v.vehicle.as_ref().and_then(|d| d.id.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| ent.id.clone());
                snap.vehicles.push(Vehicle {
                    id,
                    label: v.vehicle.as_ref().and_then(|d| d.label.clone()).filter(|s| !s.is_empty()),
                    route,
                    trip_id: v.trip.as_ref().and_then(|t| t.trip_id.clone()),
                    direction: v.trip.as_ref().and_then(|t| t.direction_id),
                    lat: v.position.as_ref().map(|p| f64::from(p.latitude)).filter(|l| l.abs() <= 90.0 && *l != 0.0),
                    lon: v.position.as_ref().map(|p| f64::from(p.longitude)).filter(|l| l.abs() <= 180.0 && *l != 0.0),
                    bearing: v.position.as_ref().and_then(|p| p.bearing).map(f64::from),
                    speed: v.position.as_ref().and_then(|p| p.speed).map(f64::from).filter(|s| s.is_finite() && *s >= 0.0),
                    status: v.current_status,
                    stop_seq: v.current_stop_sequence,
                    stop_id: v.stop_id.clone().filter(|s| !s.is_empty()),
                    occupancy: v.occupancy_status,
                    timestamp: v.timestamp,
                });
            }
            if let Some(tu) = &ent.trip_update {
                let route = tu.trip.as_ref().and_then(|t| t.route_id.clone());
                if !route_ok(routes, route.as_deref()) {
                    continue;
                }
                snap.trip_updates += 1;
                if let Some(delay_s) = trip_delay(tu) {
                    let trip_id = tu.trip.as_ref().and_then(|t| t.trip_id.clone()).unwrap_or_else(|| ent.id.clone());
                    snap.delays.push(TripDelay { trip_id, route, delay_s });
                }
            }
            if let Some(a) = &ent.alert {
                let alert_routes: Vec<String> = a.informed_entity.iter().filter_map(|e| e.route_id.clone()).collect();
                if routes.is_some() && !alert_routes.iter().any(|r| route_ok(routes, Some(r))) {
                    continue;
                }
                let text = a
                    .header_text
                    .as_ref()
                    .and_then(|t| t.translation.iter().find(|t| t.language.as_deref().is_none_or(|l| l.starts_with("en"))).or(t.translation.first()))
                    .map(|t| t.text.chars().take(140).collect::<String>());
                snap.alerts.push(AlertInfo {
                    id: ent.id.clone(),
                    cause: a.cause.unwrap_or(1),
                    effect: a.effect.unwrap_or(8),
                    routes: alert_routes,
                    informed: a.informed_entity.len(),
                    text,
                });
            }
        }
    }
    snap
}

// ---------------------------------------------------------------------------
// Normalisation + change tracking
// ---------------------------------------------------------------------------

/// How lat/lon map to `lat01`/`lon01`.
#[derive(Debug, Clone, PartialEq)]
pub enum BBox {
    Global,
    /// `[lamin, lomin, lamax, lomax]` (the opensky convention).
    Fixed([f64; 4]),
    /// Grows to the extent of the positions seen so far.
    Auto(Option<[f64; 4]>),
}

impl BBox {
    pub fn observe(&mut self, lat: f64, lon: f64) {
        if let BBox::Auto(b) = self {
            match b {
                None => *b = Some([lat, lon, lat, lon]),
                Some(b) => {
                    b[0] = b[0].min(lat);
                    b[1] = b[1].min(lon);
                    b[2] = b[2].max(lat);
                    b[3] = b[3].max(lon);
                }
            }
        }
    }
    pub fn norm(&self, lat: f64, lon: f64) -> (f64, f64) {
        let b = match self {
            BBox::Global => [-90.0, -180.0, 90.0, 180.0],
            BBox::Fixed(b) => *b,
            BBox::Auto(Some(b)) => *b,
            BBox::Auto(None) => return (0.5, 0.5),
        };
        (clamp01((lat - b[0]) / (b[2] - b[0]).max(1e-6)), clamp01((lon - b[1]) / (b[3] - b[1]).max(1e-6)))
    }
}

/// Output of one poll.
#[derive(Debug, Default)]
pub struct Update {
    pub readings: Vec<(&'static str, f64)>,
    pub events: Vec<(&'static str, f64, Attrs)>,
}

/// Remembers the previous poll to turn snapshots into `arrival`/`appeared`/`alert` events.
pub struct Tracker {
    pub bbox: BBox,
    prev_stop: HashMap<String, (Option<u32>, Option<String>)>,
    seen_vehicles: SeenSet<String>,
    seen_alerts: SeenSet<String>,
    primed: bool,
}

impl Tracker {
    pub fn new(bbox: BBox) -> Self {
        Self { bbox, prev_stop: HashMap::new(), seen_vehicles: SeenSet::new(50_000), seen_alerts: SeenSet::new(20_000), primed: false }
    }

    pub fn update(&mut self, snap: &Snapshot, now_unix: f64) -> Update {
        let mut up = Update::default();
        for v in &snap.vehicles {
            if let (Some(lat), Some(lon)) = (v.lat, v.lon) {
                self.bbox.observe(lat, lon);
            }
        }
        // Readings.
        let n = snap.vehicles.len();
        let moving = snap.vehicles.iter().filter(|v| v.moving()).count();
        let stopped = snap.vehicles.iter().filter(|v| v.stopped()).count();
        let speeds: Vec<f64> = snap.vehicles.iter().filter_map(|v| v.speed).collect();
        let routes: HashSet<&str> = snap.vehicles.iter().filter_map(|v| v.route.as_deref()).collect();
        let occ: Vec<f64> = snap.vehicles.iter().filter_map(|v| v.occupancy).filter(|o| (0..=5).contains(o)).map(|o| f64::from(o) / 5.0).collect();
        let frac = |k: usize| if n > 0 { k as f64 / n as f64 } else { 0.0 };
        let mean = |xs: &[f64]| if xs.is_empty() { 0.0 } else { xs.iter().sum::<f64>() / xs.len() as f64 };
        let delays: Vec<f64> = snap.delays.iter().map(|d| d.delay_s).collect();
        let late = if delays.is_empty() { 0.0 } else { delays.iter().filter(|d| **d > 120.0).count() as f64 / delays.len() as f64 };
        up.readings = vec![
            ("vehicles", n as f64),
            ("moving", frac(moving)),
            ("stopped", frac(stopped)),
            ("mean_speed", mean(&speeds)),
            ("routes", routes.len() as f64),
            ("trips", snap.trip_updates as f64),
            ("mean_delay", mean(&delays)),
            ("late", late),
            ("alerts", snap.alerts.len() as f64),
            ("occupancy", mean(&occ)),
        ];
        if let Some(ts) = snap.feed_ts {
            up.readings.push(("feed_age_s", (now_unix - ts as f64).max(0.0)));
        }

        // Events. The first poll only primes the memory.
        let delay_by_trip: HashMap<&str, f64> = snap.delays.iter().map(|d| (d.trip_id.as_str(), d.delay_s)).collect();
        let mut arrivals = Vec::new();
        let mut appeared = Vec::new();
        for v in &snap.vehicles {
            let (lat01, lon01) = match (v.lat, v.lon) {
                (Some(lat), Some(lon)) => {
                    let (a, b) = self.bbox.norm(lat, lon);
                    (Some(a), Some(b))
                }
                _ => (None, None),
            };
            let base = || {
                Attrs::new()
                    .text("vehicle", v.id.clone())
                    .opt_text("label", v.label.as_deref())
                    .opt_text("route", v.route.as_deref())
                    .opt_num("direction", v.direction.map(f64::from))
                    .opt_num("lat", v.lat)
                    .opt_num("lon", v.lon)
                    .opt_num("lat01", lat01)
                    .opt_num("lon01", lon01)
                    .opt_num("bearing", v.bearing)
                    .opt_num("speed", v.speed)
            };
            let is_new = self.seen_vehicles.insert(v.id.clone());
            if is_new && self.primed {
                appeared.push(("appeared", 0.5, base()));
            }
            if let Some(key) = v.stop_key() {
                let advanced = match self.prev_stop.get(&v.id) {
                    None => false,
                    Some(prev) => match (prev.0, key.0) {
                        (Some(a), Some(b)) => b > a || (b == a && prev.1 != key.1 && key.1.is_some()),
                        _ => prev.1 != key.1,
                    },
                };
                if advanced && self.primed {
                    let delay = v.trip_id.as_deref().and_then(|t| delay_by_trip.get(t).copied()).unwrap_or(0.0);
                    let intensity = (1.0 - delay / 600.0).clamp(0.2, 1.0);
                    let attrs = base()
                        .opt_text("stop", key.1.as_deref())
                        .opt_num("stop_sequence", key.0.map(f64::from))
                        .num("delay", delay)
                        .text("status", status_name(v.status.unwrap_or(STATUS_IN_TRANSIT_TO)));
                    arrivals.push(("arrival", intensity, attrs));
                }
                self.prev_stop.insert(v.id.clone(), key);
            }
        }
        // Vehicles that vanished free their memory slot.
        let live: HashSet<&str> = snap.vehicles.iter().map(|v| v.id.as_str()).collect();
        self.prev_stop.retain(|id, _| live.contains(id.as_str()));

        let mut alerts = Vec::new();
        for a in &snap.alerts {
            if self.seen_alerts.insert(a.id.clone()) && self.primed {
                let attrs = Attrs::new()
                    .text("id", a.id.clone())
                    .text("cause", cause_name(a.cause))
                    .text("effect", effect_name(a.effect))
                    .num("informed", a.informed as f64)
                    .num("routes", a.routes.len() as f64)
                    .opt_text("route", a.routes.first().map(String::as_str))
                    .opt_text("text", a.text.as_deref());
                alerts.push(("alert", effect_intensity(a.effect), attrs));
            }
        }
        self.primed = true;
        up.events.extend(arrivals);
        up.events.extend(appeared);
        up.events.extend(alerts);
        up.events.truncate(MAX_EVENTS_PER_POLL);
        up
    }
}

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

struct GtfsRtSource {
    url: String,
    trip_updates_url: Option<String>,
    alerts_url: Option<String>,
    interval: Duration,
    headers: Vec<(String, String)>,
    bbox: BBox,
    routes: Option<HashSet<String>>,
}

async fn get_feed(ctx: &SourceCtx, url: &str, headers: &[(String, String)]) -> Result<proto::FeedMessage, String> {
    let mut req = ctx
        .http
        .get(url)
        .header(reqwest::header::USER_AGENT, util::USER_AGENT)
        .header(reqwest::header::ACCEPT, "application/x-protobuf, application/octet-stream, */*")
        .timeout(util::HTTP_TIMEOUT);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.map_err(|e| format!("request failed: {}", util::short_err(&e)))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status} from {url}"));
    }
    let body = resp.bytes().await.map_err(|e| format!("body read failed: {}", util::short_err(&e)))?;
    decode_feed(&body).map_err(|e| format!("{e} ({url})"))
}

impl GtfsRtSource {
    async fn poll_once(&self, em: &mut Emitter, tracker: &mut Tracker) -> Result<(Poll, Vec<(&'static str, f64, Attrs)>), String> {
        let mut feeds = vec![get_feed(&em.ctx, &self.url, &self.headers).await?];
        for extra in [&self.trip_updates_url, &self.alerts_url].into_iter().flatten() {
            // Companion feeds are best-effort: a hiccup there must not silence the vehicles.
            match get_feed(&em.ctx, extra, &self.headers).await {
                Ok(f) => feeds.push(f),
                Err(e) => tracing::warn!(source = %em.ctx.id, url = %extra, %e, "companion feed failed"),
            }
        }
        let snap = summarize(&feeds, self.routes.as_ref());
        let up = tracker.update(&snap, util::unix_now());
        em.reading(up.readings).await;
        let live = !snap.vehicles.is_empty() || !snap.delays.is_empty() || !snap.alerts.is_empty();
        Ok((if live { Poll::Live } else { Poll::Idle }, up.events))
    }
}

impl Source for GtfsRtSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let mut em = Emitter::new(ctx);
            let mut tracker = Tracker::new(self.bbox.clone());
            // Zero interval: the poller handles status + error backoff; the success-path
            // wait is ours, so events can be spread across it.
            let mut poller = util::Poller::new(Duration::ZERO);
            poller.start(&mut em).await;
            loop {
                let started = std::time::Instant::now();
                let (res, events) = match self.poll_once(&mut em, &mut tracker).await {
                    Ok((p, ev)) => (Ok(p), ev),
                    Err(e) => (Err(e), Vec::new()),
                };
                let ok = res.is_ok();
                if !poller.after(&mut em, res).await {
                    return;
                }
                if !ok {
                    continue;
                }
                let spread = self.interval.mul_f64(0.85);
                if !events.is_empty() {
                    let gap = spread / (events.len() as u32 + 1);
                    for (name, intensity, attrs) in events {
                        if !em.sleep(gap).await {
                            return;
                        }
                        em.event(name, intensity, attrs.into_map()).await;
                    }
                }
                let remaining = self.interval.saturating_sub(started.elapsed());
                if !em.sleep(remaining).await {
                    return;
                }
            }
        })
    }
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "transit.gtfs-rt",
        summary: "GTFS-Realtime transit feed (protobuf, polled): fleet readings, `arrival`/`appeared`/`alert` events",
        params: vec![
            ParamInfo { name: "url", summary: "Vehicle positions (or combined) feed URL", default: Some(DEFAULT_URL) },
            ParamInfo { name: "trip_updates_url", summary: "Companion trip-updates feed (delays); defaults to MBTA's when `url` is unset", default: None },
            ParamInfo { name: "alerts_url", summary: "Companion service-alerts feed; defaults to MBTA's when `url` is unset", default: None },
            ParamInfo { name: "interval", summary: "Poll interval (s)", default: Some("30") },
            ParamInfo { name: "headers", summary: "Extra request headers (map), e.g. an API key header", default: None },
            ParamInfo { name: "bbox", summary: "[lamin, lomin, lamax, lomax] for lat01/lon01, or \"auto\" (fit to observed positions)", default: Some("global") },
            ParamInfo { name: "routes", summary: "Only these route ids", default: None },
        ],
        channels: vec!["vehicles", "moving", "stopped", "mean_speed", "routes", "trips", "mean_delay", "late", "alerts", "occupancy", "feed_age_s"],
        events: vec!["arrival", "appeared", "alert"],
        network: true,
    }
}

/// `headers` map → list.
pub fn parse_headers(spec: &SourceSpec) -> Result<Vec<(String, String)>, SourceError> {
    match spec.params.get("headers") {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Object(m)) => m
            .iter()
            .map(|(k, v)| {
                let k = k.trim();
                if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                    return Err(util::invalid(spec, "headers", &format!("bad header name `{k}`")));
                }
                let v = match v {
                    Json::String(s) => s.clone(),
                    Json::Number(n) => n.to_string(),
                    Json::Bool(b) => b.to_string(),
                    _ => return Err(util::invalid(spec, "headers", &format!("header `{k}` must be a string"))),
                };
                Ok((k.to_string(), v))
            })
            .collect(),
        Some(_) => Err(util::invalid(spec, "headers", "expected a map of header → value")),
    }
}

pub fn parse_bbox(spec: &SourceSpec) -> Result<BBox, SourceError> {
    match spec.params.get("bbox") {
        None | Some(Json::Null) => Ok(BBox::Global),
        Some(Json::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "fit" => Ok(BBox::Auto(None)),
            "global" | "world" | "" => Ok(BBox::Global),
            _ => Err(util::invalid(spec, "bbox", "expected [lamin, lomin, lamax, lomax] or \"auto\"")),
        },
        Some(Json::Array(a)) if a.len() == 4 => {
            let mut b = [0.0; 4];
            for (i, v) in a.iter().enumerate() {
                b[i] = util::json_f64_opt(v).ok_or_else(|| util::invalid(spec, "bbox", "expected 4 numbers"))?;
            }
            if !(b[0] < b[2] && b[1] < b[3] && b[0] >= -90.0 && b[2] <= 90.0 && b[1] >= -180.0 && b[3] <= 180.0) {
                return Err(util::invalid(spec, "bbox", "expected [lamin, lomin, lamax, lomax] with min < max"));
            }
            Ok(BBox::Fixed(b))
        }
        Some(_) => Err(util::invalid(spec, "bbox", "expected [lamin, lomin, lamax, lomax] or \"auto\"")),
    }
}

fn opt_url(spec: &SourceSpec, key: &'static str) -> Result<Option<String>, SourceError> {
    match spec.params.get(key) {
        None | Some(Json::Null) | Some(Json::Bool(false)) => Ok(None),
        Some(Json::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Json::String(s)) => {
            let s = s.trim();
            if !(s.starts_with("http://") || s.starts_with("https://")) {
                return Err(util::invalid(spec, key, "expected an http(s) URL"));
            }
            Ok(Some(s.to_string()))
        }
        Some(_) => Err(util::invalid(spec, key, "expected an http(s) URL")),
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    let explicit = opt_url(spec, "url")?;
    let default_feed = explicit.is_none();
    let url = explicit.unwrap_or_else(|| DEFAULT_URL.to_string());
    let trip_updates_url = opt_url(spec, "trip_updates_url")?.or_else(|| default_feed.then(|| DEFAULT_TRIP_UPDATES_URL.to_string()));
    let alerts_url = opt_url(spec, "alerts_url")?.or_else(|| default_feed.then(|| DEFAULT_ALERTS_URL.to_string()));
    let routes = util::param_str_list(spec, "routes")
        .map(|r| r.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<HashSet<_>>())
        .filter(|r| !r.is_empty());
    Ok(Box::new(GtfsRtSource {
        url,
        trip_updates_url,
        alerts_url,
        interval: util::param_secs(spec, "interval", 30.0, 5.0)?,
        headers: parse_headers(spec)?,
        bbox: parse_bbox(spec)?,
        routes,
    }))
}

#[cfg(test)]
mod tests {
    use super::proto::*;
    use super::*;
    use serde_json::json;

    #[allow(clippy::too_many_arguments)]
    fn vehicle(id: &str, route: &str, trip: &str, lat: f32, lon: f32, speed: Option<f32>, seq: u32, stop: &str, status: i32) -> FeedEntity {
        FeedEntity {
            id: format!("e-{id}"),
            is_deleted: None,
            trip_update: None,
            vehicle: Some(VehiclePosition {
                trip: Some(TripDescriptor { trip_id: Some(trip.into()), route_id: Some(route.into()), direction_id: Some(1), ..Default::default() }),
                vehicle: Some(VehicleDescriptor { id: Some(id.into()), label: Some(format!("car {id}")), license_plate: None }),
                position: Some(Position { latitude: lat, longitude: lon, bearing: Some(90.0), odometer: None, speed }),
                current_stop_sequence: Some(seq),
                stop_id: Some(stop.into()),
                current_status: Some(status),
                timestamp: Some(1_700_000_000),
                congestion_level: None,
                occupancy_status: Some(2),
                occupancy_percentage: None,
            }),
            alert: None,
        }
    }

    fn trip_update(trip: &str, route: &str, delay: Option<i32>, stop_delay: Option<i32>, sched: Option<i64>) -> FeedEntity {
        FeedEntity {
            id: format!("tu-{trip}"),
            is_deleted: None,
            trip_update: Some(TripUpdate {
                trip: Some(TripDescriptor { trip_id: Some(trip.into()), route_id: Some(route.into()), ..Default::default() }),
                vehicle: None,
                stop_time_update: vec![StopTimeUpdate {
                    stop_sequence: Some(3),
                    stop_id: Some("S3".into()),
                    arrival: Some(StopTimeEvent { delay: stop_delay, time: Some(1_700_000_100), uncertainty: None, scheduled_time: sched }),
                    departure: None,
                    schedule_relationship: None,
                }],
                timestamp: Some(1_700_000_000),
                delay,
            }),
            vehicle: None,
            alert: None,
        }
    }

    fn alert(id: &str, route: &str, effect: i32) -> FeedEntity {
        FeedEntity {
            id: id.into(),
            is_deleted: None,
            trip_update: None,
            vehicle: None,
            alert: Some(Alert {
                informed_entity: vec![EntitySelector { route_id: Some(route.into()), ..Default::default() }],
                cause: Some(9),
                effect: Some(effect),
                header_text: Some(TranslatedString {
                    translation: vec![Translation { text: "Shuttle buses replace service".into(), language: Some("en".into()) }],
                }),
                severity_level: None,
            }),
        }
    }

    fn feed(entities: Vec<FeedEntity>, ts: u64) -> FeedMessage {
        FeedMessage {
            header: Some(FeedHeader { gtfs_realtime_version: Some("2.0".into()), incrementality: Some(0), timestamp: Some(ts) }),
            entity: entities,
        }
    }

    #[test]
    fn roundtrip_and_tags() {
        let f = feed(vec![vehicle("v1", "Red", "t1", 42.36, -71.06, Some(8.0), 5, "S5", 2), alert("a1", "Red", 1)], 1_700_000_000);
        let bytes = f.encode_to_vec();
        let back = decode_feed(&bytes).unwrap();
        assert_eq!(back, f);
        // Hand-check a few wire tags against gtfs-realtime.proto: header is field 1
        // (0x0a), entity field 2 (0x12); inside VehiclePosition, stop_id is field 7 (0x3a).
        assert_eq!(bytes[0], 0x0a);
        assert!(bytes.contains(&0x12));
        let vp = f.entity[0].vehicle.as_ref().unwrap().encode_to_vec();
        let needle = [0x3a, 2, b'S', b'5'];
        assert!(vp.windows(4).any(|w| w == needle), "stop_id must be tag 7");
        assert!(decode_feed(b"<html>nope</html>").unwrap_err().contains("not a protobuf"));
        assert!(decode_feed(&[0xff, 0xff, 0xff]).is_err());
    }

    #[test]
    fn summary_readings_and_events_across_polls() {
        let mut t = Tracker::new(BBox::Auto(None));
        let poll1 = summarize(
            &[
                feed(vec![vehicle("v1", "Red", "t1", 42.30, -71.10, Some(8.0), 5, "S5", 2), vehicle("v2", "Blue", "t2", 42.40, -71.00, Some(0.0), 2, "S2", 1)], 1_700_000_000),
                feed(
                    vec![
                        trip_update("t1", "Red", Some(300), None, None),
                        trip_update("t2", "Blue", None, Some(30), None),
                        trip_update("t3", "Red", None, None, None),
                        trip_update("t4", "Red", None, None, Some(1_700_000_040)),
                    ],
                    1_700_000_001,
                ),
                feed(vec![alert("a1", "Red", 1)], 1_700_000_002),
            ],
            None,
        );
        assert_eq!(poll1.vehicles.len(), 2);
        assert_eq!(poll1.trip_updates, 4);
        assert_eq!(poll1.delays.len(), 3, "t3 has no delay info; t4 falls back to time − scheduled_time");
        assert_eq!(poll1.delays[2].delay_s, 60.0);
        assert_eq!(poll1.alerts.len(), 1);
        assert_eq!(poll1.feed_ts, Some(1_700_000_002));
        let up1 = t.update(&poll1, 1_700_000_010.0);
        let r: std::collections::BTreeMap<_, _> = up1.readings.iter().cloned().collect();
        assert_eq!(r["vehicles"], 2.0);
        assert_eq!(r["moving"], 0.5);
        assert_eq!(r["stopped"], 0.5);
        assert_eq!(r["trips"], 4.0);
        assert_eq!(r["mean_delay"], 130.0);
        assert!((r["late"] - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(r["alerts"], 1.0);
        assert_eq!(r["feed_age_s"], 8.0);
        assert!((r["occupancy"] - 0.4).abs() < 1e-9);
        assert!(up1.events.is_empty(), "first poll only primes");

        // Poll 2: v1 advanced a stop (late trip → softer), v2 unchanged, v3 is new, new alert.
        let poll2 = summarize(
            &[
                feed(
                    vec![
                        vehicle("v1", "Red", "t1", 42.31, -71.09, Some(8.0), 6, "S6", 2),
                        vehicle("v2", "Blue", "t2", 42.40, -71.00, Some(0.0), 2, "S2", 1),
                        vehicle("v3", "Green", "t9", 42.35, -71.05, None, 1, "S1", 2),
                    ],
                    1_700_000_030,
                ),
                feed(vec![trip_update("t1", "Red", Some(300), None, None)], 1_700_000_030),
                feed(vec![alert("a1", "Red", 1), alert("a2", "Green", 4)], 1_700_000_030),
            ],
            None,
        );
        let up2 = t.update(&poll2, 1_700_000_031.0);
        let names: Vec<&str> = up2.events.iter().map(|e| e.0).collect();
        assert_eq!(names, vec!["arrival", "appeared", "alert"]);
        let (_, intensity, attrs) = &up2.events[0];
        assert!((intensity - 0.5).abs() < 1e-9, "1 - 300/600");
        let a = attrs.clone().into_map();
        assert_eq!(a["stop"].as_str(), Some("S6"));
        assert_eq!(a["route"].as_str(), Some("Red"));
        assert_eq!(a["delay"].as_f64(), Some(300.0));
        // Auto bbox spans 42.30..42.40 / -71.10..-71.00 → v1 at (0.1, 0.1).
        assert!((a["lat01"].as_f64().unwrap() - 0.1).abs() < 1e-3, "f32 wire floats");
        assert!((a["lon01"].as_f64().unwrap() - 0.1).abs() < 1e-3);
        let alert_attrs = up2.events[2].2.clone().into_map();
        assert_eq!(alert_attrs["effect"].as_str(), Some("detour"));
        assert_eq!(alert_attrs["cause"].as_str(), Some("maintenance"));
        assert_eq!(up2.events[2].1, 0.6);
        // v3 has no speed but IN_TRANSIT_TO → moving.
        let r: std::collections::BTreeMap<_, _> = up2.readings.iter().cloned().collect();
        assert!((r["moving"] - 2.0 / 3.0).abs() < 1e-9);

        // Route filter keeps only Red.
        let red: HashSet<String> = ["Red".to_string()].into_iter().collect();
        let only = summarize(&[feed(vec![vehicle("v1", "Red", "t1", 1.0, 1.0, None, 1, "S", 2), vehicle("v2", "Blue", "t2", 1.0, 1.0, None, 1, "S", 2), alert("a2", "Green", 4)], 1)], Some(&red));
        assert_eq!(only.vehicles.len(), 1);
        assert!(only.alerts.is_empty());
    }

    #[test]
    fn bbox_and_params() {
        assert_eq!(BBox::Global.norm(0.0, 0.0), (0.5, 0.5));
        assert_eq!(BBox::Fixed([40.0, -75.0, 41.0, -73.0]).norm(40.5, -74.0), (0.5, 0.5));
        assert!(matches!(parse_bbox(&SourceSpec::new("x").with("bbox", "auto")).unwrap(), BBox::Auto(None)));
        assert!(parse_bbox(&SourceSpec::new("x").with("bbox", json!([1, 2, 3]))).is_err());
        assert!(parse_bbox(&SourceSpec::new("x").with("bbox", json!([3, 2, 1, 4]))).is_err());
        let h = parse_headers(&SourceSpec::new("x").with("headers", json!({"x-api-key": "abc"}))).unwrap();
        assert_eq!(h, vec![("x-api-key".to_string(), "abc".to_string())]);
        assert!(parse_headers(&SourceSpec::new("x").with("headers", "nope")).is_err());
        assert!(factory(&SourceSpec::new("transit.gtfs-rt")).is_ok());
        assert!(factory(&SourceSpec::new("transit.gtfs-rt").with("url", "ftp://x")).is_err());
        assert!(factory(&SourceSpec::new("transit.gtfs-rt").with("interval", 1)).is_err());
        assert!(factory(&SourceSpec::new("transit.gtfs-rt").with("url", METRO_TRANSIT_URL).with("routes", "2, 6")).is_ok());
    }

    async fn fetch(url: &str) -> Snapshot {
        let body = reqwest::Client::new().get(url).header(reqwest::header::USER_AGENT, util::USER_AGENT).send().await.unwrap().bytes().await.unwrap();
        let f = decode_feed(&body).unwrap_or_else(|e| panic!("{url}: {e}"));
        let s = summarize(std::slice::from_ref(&f), None);
        println!(
            "{url}: {} entities, {} vehicles ({} with positions), {} trip updates ({} with delays), {} alerts, header ts {:?}",
            s.entities,
            s.vehicles.len(),
            s.vehicles.iter().filter(|v| v.lat.is_some()).count(),
            s.trip_updates,
            s.delays.len(),
            s.alerts.len(),
            s.feed_ts
        );
        s
    }

    /// `cargo test -p sonder-sources -- --ignored gtfs_rt --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_mbta_default_feeds() {
        let v = fetch(DEFAULT_URL).await;
        assert!(!v.vehicles.is_empty());
        assert!(v.vehicles.iter().any(|x| x.lat.is_some() && x.route.is_some()));
        let t = fetch(DEFAULT_TRIP_UPDATES_URL).await;
        assert!(t.trip_updates > 0, "MBTA publishes predicted times, not delays — see module docs");
        let a = fetch(DEFAULT_ALERTS_URL).await;
        assert!(!a.alerts.is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn live_other_keyless_feeds() {
        let mta = fetch(MTA_SUBWAY_URL).await;
        assert!(mta.entities > 0, "MTA subway feed decodes without an API key");
        assert!(!mta.vehicles.is_empty() && mta.trip_updates > 0, "MTA combined feed has vehicles and trip updates");
        let mt = fetch(METRO_TRANSIT_URL).await;
        assert!(mt.vehicles.iter().any(|x| x.lat.is_some()));
        let mt_trips = fetch("https://svc.metrotransit.org/mtgtfs/tripupdates.pb").await;
        assert!(!mt_trips.delays.is_empty(), "Metro Transit fills StopTimeEvent.delay");
    }
}
