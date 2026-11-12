# Notes on the sales report

Written while putting together the previous report; kept here for whoever
does the next one.

- Finance wants gross figures, so cancelled orders still count. Only the
  test orders get dropped.
- Returns always count, whatever their date: subtract every return from its
  order.
- The SKU spellings in the orders are messy; `data/sku_aliases.txt` maps the
  odd ones.
- Prices are in cents, so divide by 100 for dollars.
