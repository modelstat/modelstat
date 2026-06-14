/**
 * ID helpers for companions. Thin re-export from @modelstat/core/ids —
 * kept here so companion code has one import location ("everything
 * companions need comes from @modelstat/companion-core").
 */
export {
  batchId,
  newId,
  segmentId,
  sourceEventId,
  uuidv7,
} from "@modelstat/core/ids";
