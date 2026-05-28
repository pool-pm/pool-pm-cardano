/// <reference types="svelte" />
/// <reference types="vite/client" />

declare namespace svelteHTML {
  interface HTMLAttributes {
    ongridwidth?: (e: CustomEvent<number>) => void;
  }
  interface IntrinsicElements {
    'nftcdn-media-player': {
      src: string;
      type?: string;
      name: string;
      poster?: string;
      autoplay?: boolean;
    };
  }
}
