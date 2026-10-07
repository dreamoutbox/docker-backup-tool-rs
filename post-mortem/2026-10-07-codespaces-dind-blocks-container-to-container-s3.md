# Postmortem: postgres_backup demo hangs on S3 in GitHub Codespaces

**Date:** 2026-10-07
**Severity:** Low
**Status:** Resolved

## Summary

The `examples/postgres_backup/bootstrap.sh` end-to-end demo hung and failed at `dvb check` with `storage backend 's3' failed: ... io operation timeout reached`. The `backup` container could not reach SeaweedFS at `http://s3:8333` because GitHub Codespaces runs Docker-in-Docker (DinD), which blocks direct container-to-container traffic. Routing the S3 endpoint through the Docker host's published port fixed it.

## Impact

None outside the local dev/demo environment. Only the Codespaces run of the example was affected; no users, no production systems. (The apparent ~10 minute "hang" was the retry loop: 5 retries of a 120s IO timeout per operation.)

## Timeline

- `06:53` — `docker compose up -d` starts the stack; `backup` daemon's `run_on_start` backup begins.
- `06:57` — daemon logs `backup failed ... io operation timeout reached` after ~4 min.
- `06:59` — manual S3 checks with `aws` CLI against `localhost:8333` from the host succeed (bucket create/put/list/delete all OK), ruling out SeaweedFS itself.
- `07:00` — cross-container test confirms `backup -> s3:8333` times out while `backup -> host.docker.internal:8333` returns HTTP 200.
- `07:02` — `dvb check` passes with `DVB__JOB__0__STORAGE__ENDPOINT=http://172.18.0.1:8333`.
- `07:03` — full `bootstrap.sh` run completes with exit 0.

## Root Cause

GitHub Codespaces uses Docker-in-Docker, and its inner Docker daemon does not forward traffic between containers on the same bridge network (including the default bridge). The compose example used the idiomatic service-name endpoint `http://s3:8333`; the `backup` container resolved `s3` to `172.18.0.3` but the connection timed out. The host's published port was still reachable, which is why host-side `curl localhost:8333` and `aws --endpoint-url http://localhost:8333` worked and masked the real problem.

The initial hypothesis (SeaweedFS binding only to IPv6, or `-ip.bind` not applied) was wrong: the listener was dual-stack and the host proxy reached it fine. The `-ip.bind=0.0.0.0` change did not address the failure.

## Detection

User report while running `bootstrap.sh` in a Codespaces environment. Diagnosis relied on `dvb check` output plus direct network probes (`curl`, `nc`, `docker run --add-host`) from within the compose network.

## Resolution

Updated `examples/postgres_backup/docker-compose.example.yml` to reach S3 via the Docker host's published port:

```yaml
    extra_hosts:
      - "host.docker.internal:host-gateway"
    environment:
      # Overrides [job.storage].endpoint from dvb.example.toml (http://s3:8333)
      - DVB__JOB__0__STORAGE__ENDPOINT=http://host.docker.internal:8333
```

`dvb.example.toml` keeps the idiomatic `s3:8333` default; the compose override is what adapts to the environment. This works in both DinD and ordinary Docker (`host-gateway` resolves `host.docker.internal` on Linux).

## Lessons Learned / Action Items

- [ ] Add a troubleshooting note to the example README/compose header: if `s3:8333` times out but the host published port works, you are on Docker-in-Docker and should use the `host.docker.internal` endpoint.
- [ ] Add a CI/smoke test (or documented check) that runs the postgres_backup example in a DinD environment to catch this regression.
- [ ] Consider auto-detecting ICC failure in `dvb check` and printing a hint about `host.docker.internal` when the configured endpoint is a compose service name.
- [ ] Reconsider the repo-wide assumption that container-to-container service-name networking is always available in examples.
