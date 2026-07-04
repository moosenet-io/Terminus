## Task
Create a `WordCount.java` utility in the `wx-java` workspace with word tallying
and a top-N ranking.

## Description
Add a new class `WordCount` (default package) exposing two static methods:

- `public static Map<String, Integer> count(String text)` — split `text` on
  runs of whitespace, discard empty tokens, lowercase each token, and return a
  map of word → occurrence count.
- `public static List<String> topN(Map<String, Integer> counts, int n)` — return
  the `n` highest-count words, ordered by count **descending**, with ties broken
  **lexicographically ascending**. Return at most `n` entries (all of them if
  `n` exceeds the map size); return an empty list when `n <= 0`.

## FILES
- `WordCount.java` — the new class.

## APPROACH
For `count`, split on `\s+` (trim first so a leading split doesn't yield an empty
token), lowercase, and tally into a `HashMap`. For `topN`, sort the entry set by
a comparator that orders by value descending then key ascending, then take the
first `n` keys.

## TEST PLAN
- `count("the cat the dog THE")` → `{the=3, cat=1, dog=1}`.
- `topN(counts, 2)` → `["the", "cat"]` (tie `cat` before `dog`).
- `topN(counts, 0)` → `[]`.
- `topN(counts, 10)` → `["the", "cat", "dog"]` (n exceeds size).
