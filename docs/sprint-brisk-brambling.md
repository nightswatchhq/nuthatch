# Sprint: brisk-brambling

**The drop-in is closed. The bugs that are true for everyone get fixed. Then 0048.**

Dune-mirror work (RFC-0052 S2+) waits until this sprint is done.

## Definition of done

Every issue carrying the **`brisk-brambling`** label is closed, and no open PR is for one of
them. The label, not this list, is the record of scope.

## Order

1. Close #1301 (no amendment to non-negotiable 3) and every open RFC-0053 drop-in issue.
2. #1304, then #1289. Decode and timestamps for every consumer, not for GraphQL.
3. Every remaining open `bug`.
4. RFC-0048.

Nothing else.

## Closed at filing, not in the sprint

#1301, and the 0053 drop-in track: #1257, #1284, #1266, #1267, #1268, #1212, #1213, #1306, #1307.
The GraphQL surface that already shipped stays. No further slices.

## The work

### 1. #1304 - named tuple JSON

A struct event param is stored as a positional array. The ABI has the names. `/sql` does not.
Encode a fully-named tuple as a JSON object. Unnamed stays an array. Sealed history is not
re-decoded.

### 2. #1289 - store the block timestamp

Ingestion already has the header. The store keeps the hash and drops the time. Keep both.

### 3. Open bugs

Whatever is still open and labelled `bug` after those two. At filing: #1303, #1283, #1281.

### 4. RFC-0048

Pricing query access. Tracking #1209. After the bugs, not before.
