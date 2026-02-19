#!/usr/bin/env bash
set -e
pushd /app/dbmate
dbmate wait
dbmate up
popd
