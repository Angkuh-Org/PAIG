/*
 * The two reducers are static inline in reduce.h so the NTT can inline the
 * per-butterfly reduction. This translation unit remains so the vendored
 * PQClean file layout and the build's source glob do not change.
 */
#include "reduce.h"
