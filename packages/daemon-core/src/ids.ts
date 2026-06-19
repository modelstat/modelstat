/**
 * ID helpers for daemons. Thin re-export from @modelstat/core/ids —
 * kept here so daemon code has one import location ("everything
 * daemons need comes from @modelstat/daemon-core").
 */
export {
  batchId,
  newId,
  segmentId,
  sourceEventId,
  uuidv7,
} from "@modelstat/core/ids";
