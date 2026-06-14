/**
 * Contract types shared between every companion runtime. Re-exports
 * from @modelstat/core so companions import one canonical location
 * and we don't split the type graph.
 */
export {
  HeartbeatPayload,
  IngestBatch,
  RawEvent,
  RedactionReport,
  Segment,
  TaxonomyHintRooted,
  TokenUsage,
} from "@modelstat/core/schemas";
export type {
  HeartbeatPayload as HeartbeatPayloadT,
  IngestBatch as IngestBatchT,
  RawEvent as RawEventT,
  RedactionReport as RedactionReportT,
  Segment as SegmentT,
  TaxonomyHintRooted as TaxonomyHintRootedT,
  TokenUsage as TokenUsageT,
} from "@modelstat/core/schemas";

export {
  AGENTS,
  type Agent,
  COMPANION_PHASES,
  type CompanionPhase,
  DEFAULT_TAXONOMY_ROOTS,
  PROVIDERS,
  type Provider,
  WORK_TYPES,
  type WorkType,
} from "@modelstat/core/enums";
