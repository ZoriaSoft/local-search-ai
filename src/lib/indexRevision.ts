/** A committed index/search refresh, including replies arriving before React commits. */
export interface IndexRevision {
  readonly value: number;
  isCurrent(): boolean;
}

export function createIndexRevision() {
  let revision = 0;
  const snapshot = (): IndexRevision => {
    const value = revision;
    return { value, isCurrent: () => value === revision };
  };
  return {
    snapshot,
    advance(): IndexRevision {
      revision += 1;
      return snapshot();
    },
  };
}
