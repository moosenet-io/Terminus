## Task
Create a `wordcount.go` file in the `wx-go` package with word tallying and a
top-N ranking.

## Description
Add a new file `wordcount.go` (package `wxgo`) exposing two functions:

- `func Count(text string) map[string]int` — split `text` on runs of whitespace,
  discard empty tokens, lowercase each token, and return a map of word →
  occurrence count.
- `func TopN(counts map[string]int, n int) []string` — return the `n`
  highest-count words, ordered by count **descending**, ties broken
  **lexicographically ascending**. Return at most `n` entries (all of them if
  `n` exceeds the map size); return an empty slice when `n <= 0`.

## FILES
- `wordcount.go` — the new file with both functions.

## APPROACH
For `Count`, use `strings.Fields` (which splits on whitespace and drops empties)
and `strings.ToLower`, tallying into a `map[string]int`. For `TopN`, collect the
keys into a slice, `sort.Slice` by count descending then key ascending, then take
the first `n`. Declare any new imports (`sort`, `strings`) in the new file.

## TEST PLAN
- `Count("the cat the dog THE")` → `{"the":3, "cat":1, "dog":1}`.
- `TopN(counts, 2)` → `["the", "cat"]` (tie `cat` before `dog`).
- `TopN(counts, 0)` → `[]`.
- `TopN(counts, 10)` → `["the", "cat", "dog"]` (n exceeds size).
