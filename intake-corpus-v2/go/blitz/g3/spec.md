## Task
Create a new `slug.go` file in the `wx-go` package that turns arbitrary text
into a URL-style slug.

## Description
Add a new file `slug.go` in package `wxgo` exposing a single function
`func Slugify(s string) string`.

Slugify rules:
- lowercase the input,
- replace every maximal run of characters that are NOT `[a-z0-9]` with a single
  `-` (letters/digits kept, everything else — spaces, punctuation — is a
  separator),
- strip any leading and trailing `-`.

## FILES
- `slug.go` — the new file (`package wxgo`) with the `Slugify` function.

## APPROACH
Lowercase with `strings.ToLower`, then either build the result rune-by-rune
(emitting a single `-` per separator run) or use a precompiled
`regexp.MustCompile("[^a-z0-9]+")` to replace runs with `-`, then
`strings.Trim(out, "-")`. Declare any new imports in the new file.

## TEST PLAN
- `"Hello, World!"` → `"hello-world"`.
- `"  Foo   Bar  "` → `"foo-bar"`.
- `"a--b"` → `"a-b"`.
- `"!!!"` → `""` (empty).
