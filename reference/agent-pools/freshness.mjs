export const STATUS_MAX_AGE_MS = 5 * 60 * 1000;

export function isFresh(timestamp, now = Date.now()) {
  const observed = Date.parse(timestamp);
  return Number.isFinite(observed) && observed <= now && now - observed <= STATUS_MAX_AGE_MS;
}

export function invalidateAccount(account) {
  if (!account.disabled) account.status = "unknown";
  account.usageStale = !!account.usageUpdatedAt;
  account.routingStatus = account.disabled ? "disabled" : "unknown";
  account.routingModels = {};
  account.creditsKnown = false;
  delete account.remaining;
  delete account.nextReset;
  delete account.rankReset;
  delete account.rankRemaining;
  return account;
}

export function currentAccount(account, now = Date.now()) {
  const copy = { ...account };
  if (!isFresh(copy.usageUpdatedAt, now) || copy.usageStale) invalidateAccount(copy);
  if (!isFresh(copy.routingCheckedAt, now)) {
    copy.routingStatus = copy.disabled ? "disabled" : "unknown";
    copy.routingModels = {};
  }
  if (!isFresh(copy.creditsUpdatedAt, now)) copy.creditsKnown = false;
  return copy;
}
