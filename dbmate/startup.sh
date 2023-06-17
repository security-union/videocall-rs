#!/bin/bash -e
pushd /app/dbmate
dbmate wait
dbmate up
popd
