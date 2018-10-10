#!/bin/bash
docker-compose exec spire-server sh -c "cd /opt/spire && ./spire-server entry create -parentID spiffe://example.org/simple-secrets -spiffeID spiffe://example.org/simple-secrets1 -selector unix:uid:0 -ttl 120"
docker-compose exec spire-server sh -c "cd /opt/spire && ./spire-server entry create -parentID spiffe://example.org/prometheus -spiffeID spiffe://example.org/prometheus-proxy -selector unix:uid:0 -ttl 120"
docker-compose exec spire-server sh -c "cd /opt/spire && ./spire-server entry create -parentID spiffe://example.org/fluentd -spiffeID spiffe://example.org/fluentd-proxy -selector unix:uid:0 -ttl 120"