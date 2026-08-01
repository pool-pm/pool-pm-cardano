import { describe, it, expect } from 'vitest';
import { parseMessage, verbForAction } from './cip20';

// Every message below is a real one, taken from a 30k-block mainnet window.

describe('parseMessage: dApps naming themselves', () => {
  it('reads the plain "App: Action" form', () => {
    expect(parseMessage(['Minswap: Market Order'])).toEqual({
      app: 'Minswap',
      action: 'Market Order',
      verb: 'SWAPPED',
    });
  });

  it('reads a dash separator', () => {
    expect(parseMessage(['Surf - Borrow - ADA / NIGHT'])).toMatchObject({ app: 'Surf', verb: 'BORROWED' });
  });

  it('reads a bare space separator', () => {
    expect(parseMessage(['Dexhunter Trade'])).toMatchObject({ app: 'DexHunter', verb: 'SWAPPED' });
    expect(parseMessage(['MuesliSwap Match Order'])).toMatchObject({ app: 'MuesliSwap', verb: 'EXECUTED' });
  });

  it('looks past a tooling prefix', () => {
    expect(parseMessage(['SDK Minswap: Swap Exact In Order'])).toMatchObject({ app: 'Minswap', verb: 'SWAPPED' });
  });

  it('matches a registry name the message shortens', () => {
    // The registry calls it "Splash Protocol"; on chain it writes "Splash".
    expect(parseMessage(['Splash: Swap'])).toMatchObject({ app: 'Splash', verb: 'SWAPPED' });
  });

  it('names the app even when the action is only a version', () => {
    expect(parseMessage(['CarDeM', 'SteelSwap: 1.18.0'])).toEqual({ app: 'SteelSwap' });
  });

  it('finds a dApp named after "via"', () => {
    expect(parseMessage(['Cancellation via MuesliSwap Aggregator'])).toMatchObject({
      app: 'MuesliSwap',
      verb: 'CANCELLED',
    });
  });

  it('finds a dApp named after the separator', () => {
    expect(parseMessage(['Cardano Batcher Order: MINSWAP_V2'])).toMatchObject({ app: 'Minswap' });
  });

  it('prefers the line that says what happened', () => {
    // Partner tags carry no action; the dApp's own line does.
    expect(parseMessage(['Dexhunter Trade', 'Partner VESPRiOS'])).toMatchObject({ verb: 'SWAPPED' });
  });

  it('lets a standalone cancel override the order it cancels', () => {
    expect(parseMessage(['Dexhunter Trade', 'cancel'])).toMatchObject({ app: 'DexHunter', verb: 'CANCELLED' });
  });
});

describe('parseMessage: human memos', () => {
  it.each([['donation'], ['gift'], ['Test'], ['1234'], ['Get Rugged! -HOSKY'], ['https://unfrack.it'], ['']])(
    'leaves %j alone',
    (memo) => {
      expect(parseMessage([memo])).toBeNull();
    },
  );

  it('leaves a memo that merely mentions a dApp alone', () => {
    // Anchoring at the start of the line is what prevents this.
    expect(parseMessage(['thanks for the minswap help'])).toBeNull();
  });

  it('handles no message at all', () => {
    expect(parseMessage(undefined)).toBeNull();
    expect(parseMessage([])).toBeNull();
  });
});

describe('verbForAction', () => {
  it('reads a cancellation as a cancellation, not an order', () => {
    expect(verbForAction('Aggregator Cancel Order')).toBe('CANCELLED');
  });

  it('reads an execution as an execution, not an order', () => {
    expect(verbForAction('Order Executed')).toBe('EXECUTED');
  });

  it('reads staking liquidity as a deposit, not staking', () => {
    expect(verbForAction('V2 Stake liquidity')).toBe('DEPOSITED');
  });

  it('keeps a plain limit order an order', () => {
    expect(verbForAction('Limit Order')).toBe('ORDERED');
  });

  it('has no word for an action it does not know', () => {
    expect(verbForAction('MasterChef')).toBeUndefined();
  });
});
