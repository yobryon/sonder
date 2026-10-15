// Live Earth geometry: equirectangular projection, event lat/lon extraction and a day/night
// terminator from the current UTC time (low-precision solar position, good to ~1°).

const DEG = Math.PI / 180;
const RAD = 180 / Math.PI;
const norm180 = (d) => ((((d + 180) % 360) + 360) % 360) - 180;

/** lon/lat (degrees) → canvas x/y for a W×H equirectangular world. */
export function project(lon, lat, W, H) {
  return [((lon + 180) / 360) * W, ((90 - lat) / 180) * H];
}

/** canvas x/y → lon/lat. */
export function unproject(x, y, W, H) {
  return [(x / W) * 360 - 180, 90 - (y / H) * 180];
}

/** {lat, lon} in degrees from event attrs (`lat`/`lon` or the engine's `lat01`/`lon01`). */
export function eventLatLon(attrs) {
  const a = attrs || {};
  const num = (k) => (typeof a[k] === 'number' && isFinite(a[k]) ? a[k] : null);
  const lat = num('lat'), lon = num('lon');
  if (lat !== null && lon !== null && Math.abs(lat) <= 90 && Math.abs(lon) <= 180) return { lat, lon };
  const lat01 = num('lat01'), lon01 = num('lon01');
  if (lat01 !== null && lon01 !== null) return { lat: lat01 * 180 - 90, lon: lon01 * 360 - 180 };
  return null;
}

/**
 * Subsolar point for a Date: `decl` (degrees, −23.44..23.44) and `lon` (degrees, where it is
 * local solar noon). Meeus-style low-precision formulae referenced to J2000.
 */
export function sunPosition(date) {
  const d = (date.getTime() - Date.UTC(2000, 0, 1, 12)) / 86400000; // days since J2000.0
  const L = (280.46 + 0.9856474 * d) % 360;                         // mean longitude
  const g = ((357.528 + 0.9856003 * d) % 360) * DEG;                // mean anomaly
  const lambda = (L + 1.915 * Math.sin(g) + 0.02 * Math.sin(2 * g)) * DEG; // ecliptic longitude
  const eps = (23.439 - 0.0000004 * d) * DEG;                       // obliquity
  const decl = Math.asin(Math.sin(eps) * Math.sin(lambda)) * RAD;
  const ra = Math.atan2(Math.cos(eps) * Math.sin(lambda), Math.cos(lambda)) * RAD;
  const gmst = (280.46061837 + 360.98564736629 * d) % 360;          // Greenwich sidereal, degrees
  return { decl, lon: norm180(ra - gmst) };
}

/** Sun altitude (degrees) at a point, for {decl, lon} from `sunPosition`. */
export function sunAltitude(lat, lon, sun) {
  const h = (lon - sun.lon) * DEG;
  const s = Math.sin(lat * DEG) * Math.sin(sun.decl * DEG) + Math.cos(lat * DEG) * Math.cos(sun.decl * DEG) * Math.cos(h);
  return Math.asin(Math.max(-1, Math.min(1, s))) * RAD;
}

/**
 * The night hemisphere as a polygon of [lon, lat] (degrees): the terminator sampled at `n`
 * longitudes from −180 to 180, closed over the pole that is in darkness.
 */
export function nightPolygon(sun, n = 180) {
  const decl = Math.abs(sun.decl) < 0.01 ? 0.01 * Math.sign(sun.decl || 1) : sun.decl;
  const pts = [];
  for (let i = 0; i <= n; i++) {
    const lon = -180 + (360 * i) / n;
    const h = (lon - sun.lon) * DEG;
    const lat = Math.atan(-Math.cos(h) / Math.tan(decl * DEG)) * RAD;
    pts.push([lon, lat]);
  }
  const pole = decl > 0 ? -90 : 90; // northern summer → the south pole is dark
  pts.push([180, pole], [-180, pole]);
  return pts;
}
