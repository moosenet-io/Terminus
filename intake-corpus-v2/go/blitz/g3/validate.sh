#!/usr/bin/env bash
# Validator for go-blitz-g3 (new wxgo.Slugify). Stage contract per README.
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
	cases := map[string]string{
		"Hello, World!": "hello-world",
		"  Foo   Bar  ": "foo-bar",
		"a--b":          "a-b",
		"!!!":           "",
	}
	for in, want := range cases {
		if got := Slugify(in); got != want {
			t.Fatalf("Slugify(%q): got %q want %q", in, got, want)
		}
	}
}
GO
if go test -run TestMintChange ./... >/tmp/mint_gk.$$ 2>&1; then
  echo "STAGE:CHANGE ok"; echo "PASS"; rm -f "$WORK/mint_change_test.go"; exit 0
else
  echo "STAGE:CHANGE fail"; cat /tmp/mint_gk.$$ >&2; echo "FAIL"; rm -f "$WORK/mint_change_test.go"; exit 1
fi
