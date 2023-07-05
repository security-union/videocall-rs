#!/bin/bash -e
helm upgrade --install external-dns bitnami/external-dns -f external-dns-values.yaml --set digitalocean.apiToken=$1
