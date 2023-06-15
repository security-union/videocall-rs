#!/bin/bash
pushd /app/dbmate
dbmate wait
dbmate up
popd
