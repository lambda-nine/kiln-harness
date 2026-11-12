# SKU sales report: rules

These rules define `report.txt`. They supersede any earlier notes,
including `NOTES.md`: where a note and these rules disagree, follow these
rules.

1. **Inputs.** `data/orders.csv` has the header
   `order_id,date,sku,units,unit_price_cents,status`; `data/returns.csv` has
   the header `order_id,return_date,units`; `data/sku_aliases.txt` has one
   `ALIAS CANONICAL` pair per line, separated by one space. No field
   contains a comma or a quote. Dates are `YYYY-MM-DD`; units and prices are
   non-negative integers.
2. **SKUs.** Convert every SKU to upper case. If the result equals an alias
   (aliases are compared case-insensitively), replace it with that alias's
   canonical SKU, in upper case.
3. **Statuses.** Ignore every order whose status is `test` or `cancelled`,
   in any letter case. Every other status counts.
4. **Returns.** A return reduces its order's units only when its
   `return_date` is 0 to 30 days after the order's `date`, inclusive: a
   return 30 days later counts, one 31 days later does not. Ignore returns
   dated before the order, returns for order IDs that are not in
   `orders.csv`, and returns for ignored orders. Several returns for one
   order add up. An order's net units are its units minus its counted
   returns, but never less than 0.
5. **Revenue.** An order's revenue, in cents, is its net units times its
   `unit_price_cents`.
6. **Grouping.** Add up net units and revenue per canonical SKU. Leave out
   every SKU whose total net units are 0.
7. **Format.** `report.txt` is tab-separated and every line ends in LF:
   first the header `sku<TAB>units<TAB>revenue`, then one line per SKU,
   `SKU<TAB>UNITS<TAB>REVENUE`, then `TOTAL<TAB>UNITS<TAB>REVENUE` with the
   sums over the listed SKUs. `REVENUE` is in dollars with exactly two
   decimals and no thousands separator, such as `1234.50` or `0.05`.
8. **Order.** Sort the SKU lines by revenue, highest first, and equal
   revenues by SKU in ascending ASCII order.
