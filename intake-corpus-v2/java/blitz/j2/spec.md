## Task
Add a percentage-clamp validator to the `wx-java` config validation class.

## Description
Add a `clampPercent` helper to `Validate.java` that parses a string into a
fraction in the inclusive range `[0.0, 1.0]`. It returns the parsed `double` on
success, or throws `IllegalArgumentException` for non-numeric, non-finite, or
out-of-range input.

## FILES
- `Validate.java` — add the `clampPercent` method (keep all existing methods intact).

## APPROACH
Add `public static double clampPercent(String s)`. Trim, parse with
`Double.parseDouble`, require the value to be finite (reject `NaN` / infinities)
and within `0.0..=1.0`. Throw `IllegalArgumentException` with a clear message
otherwise.

## TEST PLAN
- `"0.5"` → `0.5`; `"0"` → `0.0`; `"1"` → `1.0`.
- `"1.5"`, `"-0.1"`, `"x"` → each throws.
- The existing `sanitize` / `parsePositiveInt` methods still work.
