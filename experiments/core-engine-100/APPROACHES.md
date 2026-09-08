# Approach inventory

Each row is an executable mechanism, not an unimplemented suggestion. Variant 0
is an independent reference; variants 1–10 are the requested alternatives.
Index/bitmap/sort/partition construction is charged on every query, including
after updates. The evidence report retains losing alternatives.

## 1. scan

| Variant | Mechanism |
|---|---|
| 0 | reference row filter |
| 1 | selective predicate first |
| 2 | selection vector |
| 3 | full bitmap |
| 4 | block bitmap set bits |
| 5 | parallel block filter |
| 6 | build zone maps |
| 7 | build sorted value index |
| 8 | build low key buckets |
| 9 | columnar filter projection |
| 10 | two phase block survivors |

## 2. merge

| Variant | Mechanism |
|---|---|
| 0 | reference tree latest |
| 1 | hash latest |
| 2 | sort version reduce |
| 3 | sorted two way |
| 4 | dense version slots |
| 5 | binary patch base |
| 6 | sparse overlay map |
| 7 | block overlap search |
| 8 | parallel key ranges |
| 9 | visibility bitmap |
| 10 | copy winning runs |

## 3. low

| Variant | Mechanism |
|---|---|
| 0 | reference tree group |
| 1 | hash group |
| 2 | dense group |
| 3 | four independent dense lanes |
| 4 | sort run reduce |
| 5 | key partition hash |
| 6 | parallel local hash |
| 7 | parallel local dense |
| 8 | adaptive small map |
| 9 | group membership bitmaps |
| 10 | counting scatter groups |

## 4. high

| Variant | Mechanism |
|---|---|
| 0 | reference tree full sort |
| 1 | hash heap |
| 2 | dense heap |
| 3 | sort reduce heap |
| 4 | parallel local hash heap |
| 5 | key owned local topk |
| 6 | radix shuffle dense partials |
| 7 | adaptive domain group |
| 8 | tree quickselect |
| 9 | sorted run merge heap |
| 10 | hash quickselect |

## 5. join

| Variant | Mechanism |
|---|---|
| 0 | reference tree bucket join |
| 1 | hash bucket join |
| 2 | dense bucket join |
| 3 | sorted merge join |
| 4 | sorted binary join |
| 5 | radix partition join |
| 6 | parallel hash probe |
| 7 | parallel dense probe |
| 8 | bloom prefilter hash |
| 9 | build side demand filter |
| 10 | factorized fact aggregate |

## 6. distinct

| Variant | Mechanism |
|---|---|
| 0 | reference tree pairs |
| 1 | hash pairs |
| 2 | per group hashsets |
| 3 | sort deduplicate |
| 4 | dense group bitmaps |
| 5 | parallel local bitmaps |
| 6 | radix sort pairs |
| 7 | sorted run union |
| 8 | per group sort |
| 9 | adaptive inline sets |
| 10 | sparse word bitmaps |

## 7. topk

| Variant | Mechanism |
|---|---|
| 0 | reference full sort |
| 1 | bounded heap |
| 2 | quickselect prefix |
| 3 | sorted small vector |
| 4 | chunk local heaps |
| 5 | parallel local selection |
| 6 | value bucket selection |
| 7 | radix score order |
| 8 | tournament tree |
| 9 | block bound pruning |
| 10 | buffered selection |

## 8. window

| Variant | Mechanism |
|---|---|
| 0 | reference frame rescan |
| 1 | prefix sum monotone min |
| 2 | running sum monotone min |
| 3 | segment tree range |
| 4 | sparse table min prefix |
| 5 | two stack aggregate queue |
| 6 | block min prefix |
| 7 | parallel halo windows |
| 8 | square root range blocks |
| 9 | ordered multiset window |
| 10 | lazy min heap |

## 9. membership

| Variant | Mechanism |
|---|---|
| 0 | reference tree membership |
| 1 | hash membership |
| 2 | sorted binary membership |
| 3 | dense membership bitmap |
| 4 | bloom negative filter |
| 5 | hash partition membership |
| 6 | sorted probe merge |
| 7 | parallel hash membership |
| 8 | eytzinger search |
| 9 | radix membership |
| 10 | memoized probe outcomes |

## 10. correlated

| Variant | Mechanism |
|---|---|
| 0 | reference dependent rescan |
| 1 | memo distinct outer |
| 2 | hash decorrelation |
| 3 | dense decorrelation |
| 4 | sorted range lookup |
| 5 | key row position index |
| 6 | demand filtered aggregate |
| 7 | parallel partial decorrelation |
| 8 | parallel dependent scans |
| 9 | sorted prefix sums |
| 10 | demand bitmap dense fold |

