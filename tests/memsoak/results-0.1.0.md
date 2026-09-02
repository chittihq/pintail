# Pintail memory-churn soak

Measured 2026-09-02T08:30:43.199Z on `ghcr.io/chittihq/pintail:0.1.0` (Linux container, 2g limit).

20 tables × 200000 rows, writes of 200 rows every 200ms, 8 query clients every 500ms, supervisor every 300ms, 10 min sampled every 15s, first 3 min are warm-up.

**Verdict: FAIL.** After warm-up the per-minute memory floor went 2961 → 3157 MiB (196 MiB growth, slope 23.11 MiB/min, limits 128 MiB and 2 MiB/min); peak 3538 MiB, peak swap 1512 MiB, 459 CDC cycles.
- the process was swapped out (peak 1512 MiB in swap): it holds more than the container has
- the memory floor climbs 23.11 MiB/min after warm-up (limit 2)
- the memory floor grew 196 MiB after warm-up (limit 128)

| t (s) | RSS+swap MiB | swap MiB | cgroup MiB | CDC cycles |
|---:|---:|---:|---:|---:|
| 0 | 952 | 0 | 974 | 5 |
| 15 | 2687 | 682 | 2045 | 54 |
| 31 | 2954 | 921 | 2045 | 95 |
| 47 | 2976 | 969 | 2048 | 136 |
| 62 | 3000 | 997 | 2047 | 184 |
| 78 | 3029 | 1031 | 2047 | 235 |
| 95 | 3046 | 993 | 2048 | 279 |
| 111 | 3046 | 1017 | 2048 | 329 |
| 126 | 3002 | 981 | 2046 | 379 |
| 142 | 3064 | 1036 | 2041 | 430 |
| 158 | 2989 | 968 | 2046 | 474 |
| 174 | 3005 | 987 | 2048 | 523 |
| 190 | 2976 | 963 | 2023 | 571 |
| 205 | 2974 | 1047 | 1948 | 666 |
| 236 | 2961 | 920 | 2046 | 712 |
| 251 | 2987 | 945 | 2048 | 750 |
| 267 | 3072 | 1046 | 2045 | 789 |
| 288 | 3092 | 1051 | 2045 | 813 |
| 305 | 3151 | 1123 | 2046 | 839 |
| 321 | 3248 | 1229 | 2046 | 870 |
| 338 | 3091 | 1069 | 2038 | 895 |
| 355 | 3146 | 1147 | 2039 | 919 |
| 371 | 2933 | 983 | 1970 | 942 |
| 389 | 3047 | 1085 | 1985 | 966 |
| 405 | 3049 | 1045 | 2039 | 986 |
| 421 | 3048 | 1007 | 2044 | 1011 |
| 439 | 3110 | 1089 | 2045 | 1031 |
| 456 | 3349 | 1334 | 2029 | 1050 |
| 473 | 2947 | 966 | 2001 | 1067 |
| 489 | 3086 | 1165 | 1976 | 1087 |
| 506 | 3210 | 1180 | 2048 | 1101 |
| 522 | 3303 | 1291 | 2048 | 1116 |
| 539 | 3538 | 1512 | 2046 | 1127 |
| 555 | 3494 | 1481 | 2046 | 1141 |
| 572 | 3196 | 1313 | 1955 | 1158 |
| 588 | 3157 | 1146 | 2048 | 1171 |
