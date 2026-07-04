#!/usr/bin/env bash
# Validator for java-standard-j2 (new WordCount). Stage contract per README.
set -uo pipefail
WORK="${MINT_WORK:-$PWD}"
cd "$WORK" || { echo "STAGE:COMPILE fail"; echo "FAIL"; exit 1; }

command -v javac >/dev/null 2>&1 || { echo "TOOLCHAIN:missing javac"; exit 3; }
command -v java  >/dev/null 2>&1 || { echo "TOOLCHAIN:missing java";  exit 3; }

OUT="$WORK/_classes"; rm -rf "$OUT"; mkdir -p "$OUT"

if javac -d "$OUT" *.java >/tmp/mint_jc.$$ 2>&1; then
  echo "STAGE:COMPILE ok"
else
  echo "STAGE:COMPILE fail"; cat /tmp/mint_jc.$$ >&2; echo "FAIL"; exit 1
fi

cat > "$WORK/MintBase.java" <<'JAVA'
public class MintBase {
    static void check(boolean c, String m){ if(!c) throw new AssertionError(m); }
    public static void main(String[] a){
        check(Validate.sanitize("  hi  ").equals("hi"), "sanitize trims");
        System.out.println("BASE-OK");
    }
}
JAVA
if javac -d "$OUT" -cp "$OUT" "$WORK/MintBase.java" >/tmp/mint_jb.$$ 2>&1 && \
   java -cp "$OUT" MintBase >/dev/null 2>&1; then
  echo "STAGE:TESTS ok"
else
  echo "STAGE:TESTS fail"; cat /tmp/mint_jb.$$ >&2
fi

cat > "$WORK/MintCheck.java" <<'JAVA'
import java.util.*;
public class MintCheck {
    static void check(boolean c, String m){ if(!c) throw new AssertionError(m); }
    public static void main(String[] a){
        Map<String,Integer> c = WordCount.count("the cat the dog THE");
        check(c.get("the")==3, "the counted 3 (case-insensitive)");
        check(c.get("cat")==1 && c.get("dog")==1, "cat/dog once");
        check(c.size()==3, "three distinct words");
        check(WordCount.topN(c, 2).equals(Arrays.asList("the","cat")), "top2 with tie-break");
        check(WordCount.topN(c, 0).isEmpty(), "n=0 empty");
        check(WordCount.topN(c, 10).equals(Arrays.asList("the","cat","dog")), "n exceeds size");
        System.out.println("CHANGE-OK");
    }
}
JAVA
if javac -d "$OUT" -cp "$OUT" "$WORK/MintCheck.java" >/tmp/mint_jk.$$ 2>&1 && \
   java -cp "$OUT" MintCheck >/dev/null 2>&1; then
  echo "STAGE:CHANGE ok"; echo "PASS"; exit 0
else
  echo "STAGE:CHANGE fail"; cat /tmp/mint_jk.$$ >&2; echo "FAIL"; exit 1
fi
