// Tokens whose artwork is (near-)black ink on a transparent background. On this app's
// black wall they render as a black-on-black silhouette — effectively invisible — so their
// thumbnails are inverted, turning the ink light while leaving transparency untouched
// (`filter: invert()` doesn't affect the alpha channel).
//
// Keyed by CIP-14 fingerprint, so a policy's other assets are unaffected. Add an entry only
// after checking the art really is near-black throughout: a logo with any light element
// would come out worse inverted than left alone.
const INVERTED_ART = new Set([
  // $NIGHT (Midnight utility token): a ring + dots, every opaque pixel rgb(10,10,10).
  'asset1wd3llgkhsw6etxf2yca6cgk9ssrpva3wf0pq9a',
]);

export function hasInvertedArt(fingerprint: string): boolean {
  return INVERTED_ART.has(fingerprint);
}
