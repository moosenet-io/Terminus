## Task
Add an email-format validator to the `wx-go` package.

## Description
`validate.go` (package `wxgo`) holds stdlib-only config-field validators
(`Sanitize`, `ParsePositiveInt`). Add a `ValidateEmail` function alongside them.
It accepts a string and returns the normalized (trimmed, lowercased) address and
a `nil` error on success, or an empty string and a non-nil error on failure.

A valid email, for our purposes, must:
- contain exactly one `@`
- have a non-empty local part (before the `@`)
- have a domain (after the `@`) that contains at least one `.`
- contain no whitespace anywhere

## FILES
- `validate.go` — add `func ValidateEmail(s string) (string, error)` (keep all
  existing functions intact so the existing tests still pass).

## APPROACH
Trim the input, reject any string containing whitespace, split on `@` and
require exactly two parts, require a non-empty local part and a domain
containing `.`. Use `strings` / `fmt` (already imported). Return
`strings.ToLower` of the trimmed address on success.

## TEST PLAN
- `"<email>"` → `"<email>"`, nil error.
- No `@`, two `@`, empty local part, domain with no dot, internal space → each
  returns a non-nil error.
- Existing `Sanitize` / `ParsePositiveInt` tests still pass.
