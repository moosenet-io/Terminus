## Task
Add a percentage-clamp validator to the `wx-go` package.

## Description
Add a `ClampPercent` function to `validate.go` (package `wxgo`) that parses a
string into a fraction in the inclusive range `[0.0, 1.0]`. It returns the
parsed `float64` and a `nil` error on success, or `0` and a non-nil error for
non-numeric, non-finite, or out-of-range input.

## FILES
- `validate.go` — add `func ClampPercent(s string) (float64, error)` (keep all
  existing functions intact so the existing tests still pass).

## APPROACH
Trim, parse with `strconv.ParseFloat(s, 64)`. Reject `NaN` / infinities
(`math.IsNaN` / `math.IsInf`) and any value outside `0.0..=1.0`. Return an error
built with `fmt.Errorf` on failure. You will need to import `math` (keep the
existing imports).

## TEST PLAN
- `"0.5"` → `0.5`; `"0"` → `0.0`; `"1"` → `1.0` (all nil error).
- `"1.5"`, `"-0.1"`, `"x"` → each returns a non-nil error.
- Existing `Sanitize` / `ParsePositiveInt` tests still pass.
