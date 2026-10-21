/** An amount in cents as text: 1999 and "EUR" give "19.99 EUR". */
export function formatMoney(cents: number, currency: string): string {
  return `${(cents / 100).toFixed(2)} ${currency}`;
}
