# Reported problems

## 1. Charged for a day we didn't have

> Our account `harbor` is on the `basic` plan (30.00 a month) and started
> on 2026-01-12. We closed it with the end date 2026-03-20. Running
> `bill data 2026-03` printed this:
>
>     invoice harbor 2026-03-12 2026-04-12
>     fee basic 9/32 8.44
>     total 8.44
>
> We had the account for 8 days of that 31-day period (the 12th to the
> 19th), so we expected:
>
>     invoice harbor 2026-03-12 2026-04-12
>     fee basic 8/31 7.74
>     total 7.74

## 2. Our billing day moved to the 28th

> Our account `orchard` is on the `basic` plan and started on 2026-01-31.
> Running `bill data 2026-03` printed this:
>
>     invoice orchard 2026-03-28 2026-04-28
>     fee basic 30.00
>     total 30.00
>
> Every invoice since February has been on the 28th. We expected our
> periods to start on the last day of the month, as the manual says:
>
>     invoice orchard 2026-03-31 2026-04-30
>     fee basic 30.00
>     total 30.00
