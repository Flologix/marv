# sales-report

Turns the shop's sales export (`name,amount` per line, with a header line) into a short report: who sold how much,
largest first, and the total.

## Install

```sh
bun install
bun link
```

## Usage

```sh
report <export.csv> [--top N] [--json]
```

- `<export.csv>`: the sales export.
- `--top N`: show only the N largest rows. The total still counts every row in the file.
- `--json`: print the report as JSON, `{ "rows": [{ "name", "amount" }], "total" }`, instead of the table.
- `--limit N`: deprecated name for `--top`; still accepted, with a warning.

Example:

```sh
$ report sales.csv --top 2
ana       1200
bo         950
Total     2600
```

## Development

```sh
bun test
```
