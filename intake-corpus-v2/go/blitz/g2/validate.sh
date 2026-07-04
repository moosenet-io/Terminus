#!/usr/bin/env bash
# Validator for go-blitz-g2 (wxgo.ClampPercent). Stage contract per README.
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

import "testing"

func TestMintChange(t *testing.T) {
	cases := map[string]float64{"0.5": 0.5, "0": 0.0, "1": 1.0}
	for in, want := range cases {
		got, err := ClampPercent(in)
		if err != nil || got != want {
			t.Fatalf("ClampPercent(%q): got %v err %v want %v", in, got, err, want)
		}
	}
	for _, bad := range []string{"1.5", "-0.1", "x"} {
		if _, err := ClampPercent(bad); err == nil {
			t.Fatalf("want error for %q", bad)
		}
	}
}
GO
if go test -run TestMintChange ./... >/tmp/mint_gk.$$ 2>&1; then
  echo "STAGE:CHANGE ok"; echo "PASS"; rm -f "$WORK/mint_change_test.go"; exit 0
else
  echo "STAGE:CHANGE fail"; cat /tmp/mint_gk.$$ >&2; echo "FAIL"; rm -f "$WORK/mint_change_test.go"; exit 1
fi
