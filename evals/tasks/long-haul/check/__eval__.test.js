const EXPECTED_TESTS = "import { expect, test } from \"bun:test\";\nimport * as units from \"../src/index.js\";\n\ntest(\"format: two decimals, thousands, the unit\", () => {\n  expect(units.format(1234.5, \"km\")).toBe(\"1,234.50 km\");\n  expect(units.format(-0.004, \"h\")).toBe(\"-0.00 h\");\n  expect(units.format(0, \"MB\")).toBe(\"0.00 MB\");\n});\n\ntest(\"lengthKm\", () => {\n  expect(units.lengthKm(1234567)).toBe(\"1,234.57 km\");\n});\n\ntest(\"lengthMi\", () => {\n  expect(units.lengthMi(1986844)).toBe(\"1,234.57 mi\");\n});\n\ntest(\"massKg\", () => {\n  expect(units.massKg(1234567)).toBe(\"1,234.57 kg\");\n});\n\ntest(\"massLb\", () => {\n  expect(units.massLb(559991)).toBe(\"1,234.57 lb\");\n});\n\ntest(\"volumeL\", () => {\n  expect(units.volumeL(1234567)).toBe(\"1,234.57 L\");\n});\n\ntest(\"volumeGal\", () => {\n  expect(units.volumeGal(4673345)).toBe(\"1,234.57 gal\");\n});\n\ntest(\"timeH\", () => {\n  expect(units.timeH(4444441)).toBe(\"1,234.57 h\");\n});\n\ntest(\"timeMin\", () => {\n  expect(units.timeMin(74074)).toBe(\"1,234.57 min\");\n});\n\ntest(\"speedKmh\", () => {\n  expect(units.speedKmh(1234)).toBe(\"4,442.40 km/h\");\n});\n\ntest(\"speedMph\", () => {\n  expect(units.speedMph(1234)).toBe(\"2,760.38 mph\");\n});\n\ntest(\"areaHa\", () => {\n  expect(units.areaHa(12345670)).toBe(\"1,234.57 ha\");\n});\n\ntest(\"areaAcre\", () => {\n  expect(units.areaAcre(4996123)).toBe(\"1,234.57 acre\");\n});\n\ntest(\"energyKwh\", () => {\n  expect(units.energyKwh(4444441200)).toBe(\"1,234.57 kWh\");\n});\n\ntest(\"energyKcal\", () => {\n  expect(units.energyKcal(5165428)).toBe(\"1,234.57 kcal\");\n});\n\ntest(\"dataMb\", () => {\n  expect(units.dataMb(1234567000)).toBe(\"1,234.57 MB\");\n});\n\n";
import { expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import * as units from "./src/index.js";

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

test("every converter uses the shared format", () => {
  for (const name of readdirSync("./src/units")) {
    const text = readFileSync(`./src/units/${name}`, "utf8");
    expect(text, name).toContain("format(");
    expect(text, name).not.toContain("toFixed");
  }
});

test("the repo's own tests weren't changed", () => {
  expect(readFileSync("./test/units.test.js", "utf8")).toBe(EXPECTED_TESTS);
});
