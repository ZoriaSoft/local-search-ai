/** Keep asynchronous replies from replacing the state of a newer request. */
export function createRequestGate() {
  let generation = 0;
  return {
    next(): () => boolean {
      const request = ++generation;
      return () => request === generation;
    },
    invalidate(): void {
      generation += 1;
    },
  };
}
