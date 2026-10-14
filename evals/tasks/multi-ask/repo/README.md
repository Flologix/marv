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
report <export.csv> [--limit N]
```

- `<export.csv>`: the sales export.
- `--limit N`: show only the N largest rows. The total still counts every row in the file.

Example:

```sh
$ report sales.csv --limit 2
ana       1200
bo         950
Total     2600
```

## Development

```sh
bun test
```
