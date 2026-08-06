#!/bin/bash

set -e

export SSLKEYLOGFILE="/tmp/tmp-google/ssl_keys.log"

case $(uname) in
    (*Linux*)  google-chrome;;
    (*Darwin*) open -a "Google Chrome" --args --user-data-dir="/tmp/tmp-google" --enable-logging --v=1 ;;
esac