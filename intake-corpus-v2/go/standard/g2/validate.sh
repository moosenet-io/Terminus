#!/usr/bin/env bash
# Validator for go-standard-g2 (new wxgo.Count/TopN). Stage contract per README.
set -uo pipefail
WORK="${MINT_WORK:-$PWD}"
cd "$WORK" || { echo "STAGE:COMPILE fail"; echo "FAIL"; exit 1; }

command -v go >/dev/null 2>&1 || { echo "TOOLCHAIN:missing go"; exit 3; }

export GOCACHE="${GOCACHE:-$WORK/.gocache}"
export GOPATH="${GOPATH:-$WORK/.gopath}"
export GOFLAGS="-mod=mod"
export GOPROXY=off
export GO111MODULE=on

if go build ./... >/tmp/mint_gb.$$ 2>&1; then
  echo "STAGE:COMPILE ok"
else
  echo "STAGE:COMPILE fail"; cat /tmp/mint_gb.$$ >&2; echo "FAIL"; exit 1
fi

if go test ./... >/tmp/mint_gt.$$ 2>&1; then
  echo "STAGE:TESTS ok"
else
  echo "STAGE:TESTS fail"; cat /tmp/mint_gt.$$ >&2
fi

cat > "$WORK/mint_change_test.go" <<'GO'
package wxgo

import (
	"reflect"
	"testing"
)

func TestMintChange(t *testing.T) {
	c := Count("the cat the dog THE")
	if c["the"] != 3 || c["cat"] != 1 || c["dog"] != 1 || len(c) != 3 {
		t.Fatalf("Count: %v", c)
	}
	if got := TopN(c, 2); !reflect.DeepEqual(got, []string{"the", "cat"}) {
		t.Fatalf("TopN 2: %v", got)
	}
	if got := TopN(c, 0); len(got) != 0 {
		t.Fatalf("TopN 0: %v", got)
	}
	if got := TopN(c, 10); !reflect.DeepEqual(got, []string{"the", "cat", "dog"}) {
		t.Fatalf("TopN 10: %v", got)
	}
}
GO
if go test -run TestMintChange ./... >/tmp/mint_gk.$$ 2>&1; then
  echo "STAGE:CHANGE ok"; echo "PASS"; rm -f "$WORK/mint_change_test.go"; exit 0
else
  echo "STAGE:CHANGE fail"; cat /tmp/mint_gk.$$ >&2; echo "FAIL"; rm -f "$WORK/mint_change_test.go"; exit 1
fi
