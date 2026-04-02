/// <reference types="svelte" />
/// <reference types="vite/client" />

declare namespace svelteHTML {
  interface HTMLAttributes {
    ongridwidth?: (e: CustomEvent<number>) => void;
  }
}
