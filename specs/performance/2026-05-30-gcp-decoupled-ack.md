# 2026-05-30 GCP perf — ADR-047 decoupled-ack A/B

6-node default profile (c3-standard-22-lssd × 6 storage + × 3 clients), europe-west1-b.
Same hardware A/B: baseline = KISEKI_DECOUPLED_ACK off (cluster 1); decoupled = on (cluster 2, fresh respawn).
Driver: kiseki-client bench from kiseki-client-1, --concurrency 16 --object-size 65536 --duration-secs 20.

## Baseline (off)
```
--- baseline native put-heavy ---
{"protocol":"native-tcp","shape":"PutHeavy","concurrency":16,"object_size":65536,"duration_secs":20.061838758,"ops":12661,"errors":0,"ops_per_sec":631.0986820662793,"mib_per_sec":39.44366762914245,"p50_us":5128,"p95_us":59585,"p99_us":118113}
--- baseline native mixed ---
{"protocol":"native-tcp","shape":"Mixed","concurrency":16,"object_size":65536,"duration_secs":20.065036706,"ops":15714,"errors":0,"ops_per_sec":783.1533144068997,"mib_per_sec":48.94708215043123,"p50_us":4146,"p95_us":61903,"p99_us":115246}
--- baseline native get-heavy ---
{"protocol":"native-tcp","shape":"GetHeavy","concurrency":16,"object_size":65536,"duration_secs":20.000315286,"ops":564031,"errors":0,"ops_per_sec":28201.10542931368,"mib_per_sec":1762.569089332105,"p50_us":357,"p95_us":597,"p99_us":760}
--- baseline s3 put-heavy ---
{"protocol":"s3","shape":"PutHeavy","concurrency":16,"object_size":65536,"duration_secs":20.031033746,"ops":20120,"errors":0,"ops_per_sec":1004.441421003435,"mib_per_sec":62.77758881271469,"p50_us":4341,"p95_us":58033,"p99_us":59891}
--- baseline s3 mixed ---
{"protocol":"s3","shape":"Mixed","concurrency":16,"object_size":65536,"duration_secs":20.04920765,"ops":25996,"errors":0,"ops_per_sec":1296.6098438309107,"mib_per_sec":81.03811523943192,"p50_us":3961,"p95_us":57608,"p99_us":60979}
--- baseline s3 get-heavy ---
{"protocol":"s3","shape":"GetHeavy","concurrency":16,"object_size":65536,"duration_secs":20.000301495,"ops":543400,"errors":0,"ops_per_sec":27169.59042521674,"mib_per_sec":1698.0994015760461,"p50_us":373,"p95_us":579,"p99_us":744}
```

## Decoupled-ack (on)
```
================ DECOUPLED-ACK ON ================
--- decoupled native put-heavy ---
{"protocol":"native-tcp","shape":"PutHeavy","concurrency":16,"object_size":65536,"duration_secs":20.07883182,"ops":14287,"errors":0,"ops_per_sec":711.5453791375,"mib_per_sec":44.47158619609375,"p50_us":4340,"p95_us":60180,"p99_us":100773}
--- decoupled native mixed ---
{"protocol":"native-tcp","shape":"Mixed","concurrency":16,"object_size":65536,"duration_secs":20.044856116,"ops":16445,"errors":0,"ops_per_sec":820.4099797390634,"mib_per_sec":51.27562373369146,"p50_us":3028,"p95_us":59594,"p99_us":109530}
--- decoupled native get-heavy ---
{"protocol":"native-tcp","shape":"GetHeavy","concurrency":16,"object_size":65536,"duration_secs":20.000395571,"ops":573972,"errors":0,"ops_per_sec":28698.032394531387,"mib_per_sec":1793.6270246582117,"p50_us":364,"p95_us":542,"p99_us":678}
--- decoupled s3 put-heavy ---
{"protocol":"s3","shape":"PutHeavy","concurrency":16,"object_size":65536,"duration_secs":20.023806353,"ops":19978,"errors":0,"ops_per_sec":997.7124053143303,"mib_per_sec":62.357025332145646,"p50_us":6930,"p95_us":52942,"p99_us":62130}
--- decoupled s3 mixed ---
{"protocol":"s3","shape":"Mixed","concurrency":16,"object_size":65536,"duration_secs":20.013986074,"ops":28245,"errors":0,"ops_per_sec":1411.26309849355,"mib_per_sec":88.20394365584687,"p50_us":4103,"p95_us":50720,"p99_us":59699}
--- decoupled s3 get-heavy ---
```
