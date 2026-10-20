/** Game version folders the app has guides for, the preferred one of a version first (WoW
 * Forever's beta is in _classic_beta_, its stress test made _forever_, the folder after its
 * release). */
export const SUPPORTED = ["_forever_", "_classic_beta_", "_classic_era_", "_anniversary_"];

/** Known game version folders. */
export const CLIENTS: Record<string, string> = {
  _forever_: "WoW Forever",
  _classic_beta_: "WoW Forever (beta)",
  _classic_era_: "Classic Era",
  _anniversary_: "TBC Anniversary",
  _classic_: "Mists of Pandaria Classic",
  _retail_: "Retail",
  _classic_era_ptr_: "Classic Era PTR",
  _classic_ptr_: "Classic PTR",
  _ptr_: "PTR",
  _beta_: "Beta",
};
