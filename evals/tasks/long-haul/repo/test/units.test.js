import { expect, test } from "bun:test";
import * as units from "../src/index.js";

test("format: two decimals, thousands, the unit", () => {
  expect(units.format(1234.5, "km")).toBe("1,234.50 km");
  expect(units.format(-0.004, "h")).toBe("-0.00 h");
  expect(units.format(0, "MB")).toBe("0.00 MB");
});

test("lengthKm", () => {
  expect(units.lengthKm(1234567)).toBe("1,234.57 km");
});

test("lengthMi", () => {
  expect(units.lengthMi(1986844)).toBe("1,234.57 mi");
});

test("massKg", () => {
  expect(units.massKg(1234567)).toBe("1,234.57 kg");
});

test("massLb", () => {
  expect(units.massLb(559991)).toBe("1,234.57 lb");
});

test("volumeL", () => {
  expect(units.volumeL(1234567)).toBe("1,234.57 L");
});

test("volumeGal", () => {
  expect(units.volumeGal(4673345)).toBe("1,234.57 gal");
});

test("timeH", () => {
  expect(units.timeH(4444441)).toBe("1,234.57 h");
});

test("timeMin", () => {
  expect(units.timeMin(74074)).toBe("1,234.57 min");
});

test("speedKmh", () => {
  expect(units.speedKmh(1234)).toBe("4,442.40 km/h");
});

test("speedMph", () => {
  expect(units.speedMph(1234)).toBe("2,760.38 mph");
});

test("areaHa", () => {
  expect(units.areaHa(12345670)).toBe("1,234.57 ha");
});

test("areaAcre", () => {
  expect(units.areaAcre(4996123)).toBe("1,234.57 acre");
});

test("energyKwh", () => {
  expect(units.energyKwh(4444441200)).toBe("1,234.57 kWh");
});

test("energyKcal", () => {
  expect(units.energyKcal(5165428)).toBe("1,234.57 kcal");
});

test("dataMb", () => {
  expect(units.dataMb(1234567000)).toBe("1,234.57 MB");
});

