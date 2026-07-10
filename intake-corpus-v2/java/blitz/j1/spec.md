## Task
Add an email-format validator to the `wx-java` config validation class.

## Description
`Validate.java` holds static config-field validators (`sanitize`,
`parsePositiveInt`). Add a `validateEmail` helper alongside them. It accepts a
string and returns the normalized (trimmed, lowercased) address on success, or
throws `IllegalArgumentException` on failure.

A valid email, for our purposes, must:
- contain exactly one `@`
- have a non-empty local part (before the `@`)
- have a domain (after the `@`) that contains at least one `.`
- contain no whitespace anywhere

## FILES
- `Validate.java` — add the `validateEmail` method (keep all existing methods intact).

## APPROACH
Mirror the style of the existing validators. Add a public static method
`public static String validateEmail(String s)`. Trim first, then run the checks,
throwing `IllegalArgumentException` with a clear message on any failure. Return
the trimmed + lowercased address on success.

## TEST PLAN
- `"<email>"` → `"<email>"` (normalized). <!-- pii-test-fixture -->
- No `@`, two `@`, empty local part, and a domain with no dot → each throws.
- A string containing an internal space → throws.
- The existing `sanitize` / `parsePositiveInt` methods still work.
