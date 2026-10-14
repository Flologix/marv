// Formatting helpers for the dashboard. Every formatter returns "—" for values it can't show.

/** Bytes, with 1 decimal. */
export function formatBytes(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1).toFixed(1)} B`;
}

/** Bytes, rounded, for compact table cells. */
export function formatBytesShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1)}B`;
}

/** A range of Bytes, like "1.5–3.0 B". */
export function formatBytesRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1).toFixed(1)}–${(high / 1).toFixed(1)} B`;
}

/** Kilobytes, with 2 decimals. */
export function formatKilobytes(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1024).toFixed(2)} KB`;
}

/** Kilobytes, rounded, for compact table cells. */
export function formatKilobytesShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1024)}KB`;
}

/** A range of Kilobytes, like "1.5–3.0 KB". */
export function formatKilobytesRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1024).toFixed(2)}–${(high / 1024).toFixed(2)} KB`;
}

/** Megabytes, with 3 decimals. */
export function formatMegabytes(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1048576).toFixed(3)} MB`;
}

/** Megabytes, rounded, for compact table cells. */
export function formatMegabytesShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1048576)}MB`;
}

/** A range of Megabytes, like "1.5–3.0 MB". */
export function formatMegabytesRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1048576).toFixed(3)}–${(high / 1048576).toFixed(3)} MB`;
}

/** Gigabytes, with 1 decimal. */
export function formatGigabytes(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1073741824).toFixed(1)} GB`;
}

/** Gigabytes, rounded, for compact table cells. */
export function formatGigabytesShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1073741824)}GB`;
}

/** A range of Gigabytes, like "1.5–3.0 GB". */
export function formatGigabytesRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1073741824).toFixed(1)}–${(high / 1073741824).toFixed(1)} GB`;
}

/** Terabytes, with 2 decimals. */
export function formatTerabytes(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1099511627776).toFixed(2)} TB`;
}

/** Terabytes, rounded, for compact table cells. */
export function formatTerabytesShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1099511627776)}TB`;
}

/** A range of Terabytes, like "1.5–3.0 TB". */
export function formatTerabytesRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1099511627776).toFixed(2)}–${(high / 1099511627776).toFixed(2)} TB`;
}

/** Millimeters, with 3 decimals. */
export function formatMillimeters(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 0.001).toFixed(3)} mm`;
}

/** Millimeters, rounded, for compact table cells. */
export function formatMillimetersShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 0.001)}mm`;
}

/** A range of Millimeters, like "1.5–3.0 mm". */
export function formatMillimetersRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 0.001).toFixed(3)}–${(high / 0.001).toFixed(3)} mm`;
}

/** Centimeters, with 1 decimal. */
export function formatCentimeters(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 0.01).toFixed(1)} cm`;
}

/** Centimeters, rounded, for compact table cells. */
export function formatCentimetersShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 0.01)}cm`;
}

/** A range of Centimeters, like "1.5–3.0 cm". */
export function formatCentimetersRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 0.01).toFixed(1)}–${(high / 0.01).toFixed(1)} cm`;
}

/** Meters, with 2 decimals. */
export function formatMeters(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1).toFixed(2)} m`;
}

/** Meters, rounded, for compact table cells. */
export function formatMetersShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1)}m`;
}

/** A range of Meters, like "1.5–3.0 m". */
export function formatMetersRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1).toFixed(2)}–${(high / 1).toFixed(2)} m`;
}

/** Kilometers, with 3 decimals. */
export function formatKilometers(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1000).toFixed(3)} km`;
}

/** Kilometers, rounded, for compact table cells. */
export function formatKilometersShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1000)}km`;
}

/** A range of Kilometers, like "1.5–3.0 km". */
export function formatKilometersRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1000).toFixed(3)}–${(high / 1000).toFixed(3)} km`;
}

/** Miles, with 1 decimal. */
export function formatMiles(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1609.344).toFixed(1)} mi`;
}

/** Miles, rounded, for compact table cells. */
export function formatMilesShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1609.344)}mi`;
}

/** A range of Miles, like "1.5–3.0 mi". */
export function formatMilesRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1609.344).toFixed(1)}–${(high / 1609.344).toFixed(1)} mi`;
}

/** Grams, with 2 decimals. */
export function formatGrams(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1).toFixed(2)} g`;
}

/** Grams, rounded, for compact table cells. */
export function formatGramsShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1)}g`;
}

/** A range of Grams, like "1.5–3.0 g". */
export function formatGramsRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1).toFixed(2)}–${(high / 1).toFixed(2)} g`;
}

/** Kilograms, with 3 decimals. */
export function formatKilograms(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1000).toFixed(3)} kg`;
}

/** Kilograms, rounded, for compact table cells. */
export function formatKilogramsShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1000)}kg`;
}

/** A range of Kilograms, like "1.5–3.0 kg". */
export function formatKilogramsRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1000).toFixed(3)}–${(high / 1000).toFixed(3)} kg`;
}

/** Pounds, with 1 decimal. */
export function formatPounds(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 453.592).toFixed(1)} lb`;
}

/** Pounds, rounded, for compact table cells. */
export function formatPoundsShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 453.592)}lb`;
}

/** A range of Pounds, like "1.5–3.0 lb". */
export function formatPoundsRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 453.592).toFixed(1)}–${(high / 453.592).toFixed(1)} lb`;
}

/** Ounces, with 2 decimals. */
export function formatOunces(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 28.3495).toFixed(2)} oz`;
}

/** Ounces, rounded, for compact table cells. */
export function formatOuncesShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 28.3495)}oz`;
}

/** A range of Ounces, like "1.5–3.0 oz". */
export function formatOuncesRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 28.3495).toFixed(2)}–${(high / 28.3495).toFixed(2)} oz`;
}

/** Tonnes, with 3 decimals. */
export function formatTonnes(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1000000.0).toFixed(3)} t`;
}

/** Tonnes, rounded, for compact table cells. */
export function formatTonnesShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1000000.0)}t`;
}

/** A range of Tonnes, like "1.5–3.0 t". */
export function formatTonnesRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1000000.0).toFixed(3)}–${(high / 1000000.0).toFixed(3)} t`;
}

/** Milliliters, with 1 decimal. */
export function formatMilliliters(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1).toFixed(1)} ml`;
}

/** Milliliters, rounded, for compact table cells. */
export function formatMillilitersShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1)}ml`;
}

/** A range of Milliliters, like "1.5–3.0 ml". */
export function formatMillilitersRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1).toFixed(1)}–${(high / 1).toFixed(1)} ml`;
}

/** Liters, with 2 decimals. */
export function formatLiters(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1000).toFixed(2)} l`;
}

/** Liters, rounded, for compact table cells. */
export function formatLitersShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1000)}l`;
}

/** A range of Liters, like "1.5–3.0 l". */
export function formatLitersRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1000).toFixed(2)}–${(high / 1000).toFixed(2)} l`;
}

/** Gallons, with 3 decimals. */
export function formatGallons(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 3785.41).toFixed(3)} gal`;
}

/** Gallons, rounded, for compact table cells. */
export function formatGallonsShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 3785.41)}gal`;
}

/** A range of Gallons, like "1.5–3.0 gal". */
export function formatGallonsRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 3785.41).toFixed(3)}–${(high / 3785.41).toFixed(3)} gal`;
}

/** Celsius, with 1 decimal. */
export function formatCelsius(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1).toFixed(1)} °C`;
}

/** Celsius, rounded, for compact table cells. */
export function formatCelsiusShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1)}°C`;
}

/** A range of Celsius, like "1.5–3.0 °C". */
export function formatCelsiusRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1).toFixed(1)}–${(high / 1).toFixed(1)} °C`;
}

/** Kelvin, with 2 decimals. */
export function formatKelvin(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1).toFixed(2)} K`;
}

/** Kelvin, rounded, for compact table cells. */
export function formatKelvinShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1)}K`;
}

/** A range of Kelvin, like "1.5–3.0 K". */
export function formatKelvinRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1).toFixed(2)}–${(high / 1).toFixed(2)} K`;
}

/** Watts, with 3 decimals. */
export function formatWatts(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1).toFixed(3)} W`;
}

/** Watts, rounded, for compact table cells. */
export function formatWattsShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1)}W`;
}

/** A range of Watts, like "1.5–3.0 W". */
export function formatWattsRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1).toFixed(3)}–${(high / 1).toFixed(3)} W`;
}

/** Kilowatts, with 1 decimal. */
export function formatKilowatts(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1000).toFixed(1)} kW`;
}

/** Kilowatts, rounded, for compact table cells. */
export function formatKilowattsShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1000)}kW`;
}

/** A range of Kilowatts, like "1.5–3.0 kW". */
export function formatKilowattsRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1000).toFixed(1)}–${(high / 1000).toFixed(1)} kW`;
}

/** Hertz, with 2 decimals. */
export function formatHertz(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1).toFixed(2)} Hz`;
}

/** Hertz, rounded, for compact table cells. */
export function formatHertzShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1)}Hz`;
}

/** A range of Hertz, like "1.5–3.0 Hz". */
export function formatHertzRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1).toFixed(2)}–${(high / 1).toFixed(2)} Hz`;
}

/** Kilohertz, with 3 decimals. */
export function formatKilohertz(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1000).toFixed(3)} kHz`;
}

/** Kilohertz, rounded, for compact table cells. */
export function formatKilohertzShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1000)}kHz`;
}

/** A range of Kilohertz, like "1.5–3.0 kHz". */
export function formatKilohertzRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1000).toFixed(3)}–${(high / 1000).toFixed(3)} kHz`;
}

/** A number of seconds as "1h 2m 5s" (leaving out zero hours and minutes). */
export function formatDuration(seconds) {
  if (!Number.isFinite(seconds) || seconds < 0) return "—";
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const rest = Math.floor(seconds % 60);
  const parts = [];
  if (hours) parts.push(`${hours}h`);
  if (hours || minutes) parts.push(`${minutes}m`);
  parts.push(`${rest}s`);
  return parts.join(" ");
}

/** Megahertz, with 1 decimal. */
export function formatMegahertz(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1000000.0).toFixed(1)} MHz`;
}

/** Megahertz, rounded, for compact table cells. */
export function formatMegahertzShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1000000.0)}MHz`;
}

/** A range of Megahertz, like "1.5–3.0 MHz". */
export function formatMegahertzRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1000000.0).toFixed(1)}–${(high / 1000000.0).toFixed(1)} MHz`;
}

/** Pixels, with 2 decimals. */
export function formatPixels(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1).toFixed(2)} px`;
}

/** Pixels, rounded, for compact table cells. */
export function formatPixelsShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1)}px`;
}

/** A range of Pixels, like "1.5–3.0 px". */
export function formatPixelsRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1).toFixed(2)}–${(high / 1).toFixed(2)} px`;
}

/** Points, with 3 decimals. */
export function formatPoints(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1).toFixed(3)} pt`;
}

/** Points, rounded, for compact table cells. */
export function formatPointsShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1)}pt`;
}

/** A range of Points, like "1.5–3.0 pt". */
export function formatPointsRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1).toFixed(3)}–${(high / 1).toFixed(3)} pt`;
}

/** Percent, with 1 decimal. */
export function formatPercent(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 0.01).toFixed(1)} %`;
}

/** Percent, rounded, for compact table cells. */
export function formatPercentShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 0.01)}%`;
}

/** A range of Percent, like "1.5–3.0 %". */
export function formatPercentRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 0.01).toFixed(1)}–${(high / 0.01).toFixed(1)} %`;
}

/** Degrees, with 2 decimals. */
export function formatDegrees(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1).toFixed(2)} °`;
}

/** Degrees, rounded, for compact table cells. */
export function formatDegreesShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1)}°`;
}

/** A range of Degrees, like "1.5–3.0 °". */
export function formatDegreesRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1).toFixed(2)}–${(high / 1).toFixed(2)} °`;
}

/** Radians, with 3 decimals. */
export function formatRadians(value) {
  if (!Number.isFinite(value)) return "—";
  return `${(value / 1).toFixed(3)} rad`;
}

/** Radians, rounded, for compact table cells. */
export function formatRadiansShort(value) {
  if (!Number.isFinite(value)) return "—";
  return `${Math.round(value / 1)}rad`;
}

/** A range of Radians, like "1.5–3.0 rad". */
export function formatRadiansRange(low, high) {
  if (!Number.isFinite(low) || !Number.isFinite(high)) return "—";
  return `${(low / 1).toFixed(3)}–${(high / 1).toFixed(3)} rad`;
}
