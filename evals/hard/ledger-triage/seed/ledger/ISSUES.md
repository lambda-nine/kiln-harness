# Open issues

Reported by users. Each has a journal and a command to reproduce it.

## 1. A prefix picks up the wrong accounts

I keep a court escrow account next to my bank account, and asking for the
bank's balance counts the escrow too.

`escrow.journal`:

```text
2026-05-01 Deposit
    Assets:Bank         $100.00
    Income:Gift

2026-05-02 Court escrow
    Assets:Bankruptcy    $40.00
    Income:Gift
```

`ledger balance escrow.journal Assets:Bank` prints

```text
       $140.00  Assets
       $100.00    Bank
        $40.00    Bankruptcy
--------------
       $140.00
```

but I expected

```text
       $100.00  Assets
       $100.00    Bank
--------------
       $100.00
```

`ledger register escrow.journal Assets:Bank` lists the escrow deposit as
well.

## 2. Big amounts come out a thousand times too small

`house.journal`:

```text
2026-06-15 House purchase
    Assets:House          $1,250,000.00
    Liabilities:Mortgage  $-1,000,000.00
    Assets:Bank:Checking
```

`ledger balance house.journal` prints

```text
     $1,000.00  Assets
      $-250.00    Bank
      $-250.00      Checking
     $1,250.00    House
    $-1,000.00  Liabilities
    $-1,000.00    Mortgage
--------------
         $0.00
```

but I expected

```text
 $1,000,000.00  Assets
  $-250,000.00    Bank
  $-250,000.00      Checking
 $1,250,000.00    House
$-1,000,000.00  Liabilities
$-1,000,000.00    Mortgage
--------------
         $0.00
```

Smaller amounts such as `$12,000.00` are fine.

## 3. check points at the wrong line

My journals are long, and when `check` finds an unbalanced transaction I
look for it at the line it gives, but that is never where the transaction
starts.

`paint.journal`:

```text
; March
2026-03-10 Hardware store
    Expenses:Tools   $25.00
    Expenses:Paint   $14.99
    Assets:Cash     $-39.00
```

`ledger check paint.journal` prints `unbalanced transaction at line 5`
and exits with status 1. The status is right, but the manual says the line
should be the transaction's header, so I expected
`unbalanced transaction at line 2`.
