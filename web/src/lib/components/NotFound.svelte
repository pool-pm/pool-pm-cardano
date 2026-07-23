<script lang="ts">
  // A generic "not found" view for a URL that doesn't resolve to a real subject:
  // a malformed/unknown address, an unresolvable $handle, an empty policy, a
  // nonexistent asset, … The global search bar + home logo (rendered by App) stay
  // available, so this only needs the message and a way back.
  let { title = 'Not found', detail = '' }: { title?: string; detail?: string } = $props();
</script>

<div class="notfound">
  <div class="code">404</div>
  <div class="title">{title}</div>
  {#if detail}
    <div class="detail">{detail}</div>
  {/if}
  <a class="home" href="/">← back to pool.pm</a>
</div>

<style>
  .notfound {
    position: fixed;
    inset: 0;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 10px;
    padding: 24px;
    text-align: center;
    color: #e3e3e3;
  }
  .code {
    font-size: 64px;
    font-weight: 800;
    line-height: 1;
    color: #9c27b0;
  }
  .title {
    font-size: 20px;
    font-weight: 600;
  }
  .detail {
    max-width: min(90vw, 640px);
    font-size: 13px;
    color: #999;
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    word-break: break-all;
  }
  .home {
    margin-top: 12px;
    font-size: 14px;
    color: #bbb;
    text-decoration: none;
  }
  .home:hover {
    color: #e3e3e3;
  }
</style>
