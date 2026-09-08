# Third-party brand assets

Official packs, kept here **on purpose and not in `public/`**: everything under `public/`
is copied into every app bundle on all five platforms, and these are 263 files and 3.1 MB
of material we use six pieces of. What the app actually shows is **inlined as SVG** in
`public/index.html`, so at runtime none of these files is needed.

| | |
|---|---|
| `Mollie_Payment_Methods/` | Mollie's payment-method marks. We use the `-card` (64×48) variants of **Visa, Mastercard, Amex, Apple-pay, Google-Pay** in the buy sheet's card row. The `-squircle` (64×64) variants are the alternative shape; we do not use them. |
| `Mollie_Logo/` | The 2023 wordmark. The buy sheet carries it under "Processed by". |

## Rules for using them

**Never redraw a third-party mark by hand.** Take the path out of the official file. A
wordmark drawn from memory is wrong in ways that are hard to see and impossible to defend.

**The Mollie wordmark's black and white files are the same path** — they differ only in
`fill`. That is why `index.html` carries one path and switches the fill between `#FFFFFF`
and `#000000` per theme: the result is exactly the official pair, not a recolouring, which
a brand guideline would not permit.

**Keep the card marks at their own aspect ratio** (4:3 for `-card`). Set a height in CSS and
let the width follow; squeezing a card mark is the most common way to get this wrong.

## Adding a method later

When a method is switched on in the Mollie dashboard, `GET /v2/methods` starts returning it
and the app's tile label follows on its own (`mollie_methods()` → `cardLabelShort()`). The
**mark** does not: pull the matching `-card` SVG out of the folder above and inline it next
to the others in the card row.
