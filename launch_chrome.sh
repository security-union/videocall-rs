#!/bin/bash

set -e

SCRIPTPATH="$( cd -- "$(dirname "$0")" >/dev/null 2>&1 ; pwd -P )"
CERTSPATH="$SCRIPTPATH/actix-api/certs"

if ! [ -f "$CERTSPATH/localhost.der" ] ; then
    echo "Generating certificate in $CERTSPATH"
    openssl req -x509 -newkey rsa:2048 -keyout "$CERTSPATH/localhost.key" -out "$CERTSPATH/localhost.pem" -days 365 -nodes -subj "/CN=127.0.0.1"
    openssl x509 -in "$CERTSPATH/localhost.pem" -outform der -out "$CERTSPATH/localhost.der"
    openssl rsa -in "$CERTSPATH/localhost.key" -outform DER -out "$CERTSPATH/localhost_key.der"
fi

SPKI=$(openssl x509 -inform der -in "$CERTSPATH/localhost.der" -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | openssl enc -base64)

echo "Opening google chrome"

case $(uname) in
    (*Linux*)  google-chrome --origin-to-force-quic-on=127.0.0.1:4433 --ignore-certificate-errors-spki-list="$SPKI" --enable-logging --v=1 ;;
    (*Darwin*)  open -a "Google Chrome" --args --origin-to-force-quic-on=127.0.0.1:4433 --ignore-certificate-errors-spki-list="$SPKI" --enable-logging --v=1 ;;
esac

## Logs are stored to ~/Library/Application Support/Google/Chrome/chrome_debug.log
