import { useCallback, useEffect, useState } from "react";
import { api, type CharacterInfo, type GuideSummary, type Options } from "../api";
import { useAddonHealth } from "../components/AddonInGame";
import { ClassName, FactionIcon, raceFaction } from "../components/Character";
import { Hint } from "../components/Hint";
import { Icon } from "../components/Icons";
import { PROFESSIONS } from "../components/ProfessionPicker";
import { useDate, useGameName, useT } from "../i18n";

interface Props {
  /** Options of the game version (race names and factions), when loaded. */
  options: Options | null;
  /** Show a guide (in the version its characters follow). */
  onOpenGuide: (guide: string, version: number) => void;
  /** Start a new guide from a character (it follows the guide once computed). */
  onNewGuide: (character: string) => void;
  /** Name of the game version chosen in the menu. */
  editionName: string;
  /** Go to the addon page (to install it). */
  onAddon: () => void;
}

/** The latest version of a guide, and the one its characters follow. */
const latest = (g: GuideSummary) => g.versions[0];
const followed = (g: GuideSummary) => g.versions.find((v) => v.version === g.installed) ?? latest(g);

/**
 * The account's characters the addon recorded, one block each: who it is, where it is, and the
 * guide it follows (0 or 1, chosen here only: the addon shows it in game after /reload).
 */
export function CharactersPage({ options, onOpenGuide, onNewGuide, editionName, onAddon }: Props) {
  const health = useAddonHealth();
  const t = useT();
  const date = useDate();
  const gameName = useGameName();
  const [characters, setCharacters] = useState<CharacterInfo[] | null>(null);
  const [guides, setGuides] = useState<GuideSummary[]>([]);
  const [assignments, setAssignments] = useState<Record<string, string>>({});
  // Every guide offered to every character (a setting), else those of its race and class.
  const [allGuides, setAllGuides] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(() => {
    api.listCharacters().then((list) => setCharacters(list.filter((c) => !c.shared))).catch((e) => setError(String(e)));
    api.listGuides().then(setGuides).catch((e) => setError(String(e)));
    api.listAssignments().then(setAssignments).catch((e) => setError(String(e)));
    api
      .getSettings()
      .then((s) => setAllGuides(Boolean(s.all_guides)))
      .catch(() => {});
  }, []);
  useEffect(load, [load]);

  const assign = async (character: string, guide: string | null) => {
    setError(null);
    setMessage(null);
    try {
      const n = await api.assignGuide(character, guide);
      setAssignments((a) => {
        const next = { ...a };
        if (guide) next[character] = guide;
        else delete next[character];
        return next;
      });
      setMessage(t("Guides dans l'addon : {n} — faites /reload en jeu", { n }));
    } catch (e) {
      setError(String(e));
    }
  };

  // A character deleted in game: the game keeps its files and the addon its profile.
  const forget = async (character: string, name: string) => {
    if (!confirm(t("Retirer {name} de la liste ? Pour un personnage supprimé en jeu : il revient s'il est joué de nouveau.", { name }))) return;
    setError(null);
    setMessage(null);
    try {
      await api.forgetCharacter(character);
      setCharacters((list) => list?.filter((c) => c.profile.name !== character) ?? null);
      setAssignments((a) => {
        const next = { ...a };
        delete next[character];
        return next;
      });
    } catch (e) {
      setError(String(e));
    }
  };

  const raceName = (race: string | null) => (race ? gameName(options?.races.find((r) => r.key === race)?.name ?? race) : "");
  const professionName = (key: string) => t(PROFESSIONS.find((p) => p.key === key)?.label ?? key);

  return (
    <div className="characters-page">
      <div className="page-head">
        <h2>
          {t("Personnages")}
          <Hint
            text={`${t("Chaque personnage suit au plus un guide, choisi ici : l'addon le lui montre en jeu après /reload.")} ${t("Un personnage manque ? Connectez-vous avec lui en jeu, puis /reload.")}`}
          />
        </h2>
      </div>
      {message && <div className="success">{message}</div>}
      {error && <div className="error">{error}</div>}
      {characters === null ? (
        <div className="empty">{t("Chargement…")}</div>
      ) : characters.length === 0 ? (
        <section className="panel characters-empty">
          <h3>{t("Aucun personnage pour {edition}", { edition: editionName })}</h3>
          {health && (
            <p className="likely-cause">
              <strong>{t("Cause probable :")}</strong>{" "}
              {!health.installed_version
                ? t("l'addon n'est pas encore installé dans {edition}.", { edition: editionName })
                : health.not_loaded
                  ? t("le jeu a tourné sans lancer l'addon : à l'écran de sélection du personnage, bouton AddOns, cochez Factoruide.")
                  : health.disabled_for.length
                    ? t("l'addon est désactivé dans la liste des AddOns du jeu pour : {list}.", { list: health.disabled_for.join(", ") })
                    : !health.seen
                      ? t("l'addon n'a pas encore tourné en jeu : connectez un personnage, puis faites /reload ou déconnectez-vous.")
                      : t("le personnage est sur une autre version du jeu, ou il n'a pas encore été enregistré (faites /reload en jeu).")}
            </p>
          )}
          <p>{t("Un personnage apparaît ici une fois enregistré par l'addon. S'il n'y en a pas :")}</p>
          <ul>
            <li>
              <strong>{t("Mauvaise version du jeu")}</strong> — {t("le personnage est sur une autre version que celle choisie en haut du menu ({edition}).", { edition: editionName })}
            </li>
            <li>
              <strong>{t("Addon pas installé")}</strong> — {t("installez-le dans ce client depuis la page Addon.")}{" "}
              <button className="link" onClick={onAddon}>
                {t("Aller à la page Addon")}
              </button>
            </li>
            <li>
              <strong>{t("Personnage pas encore enregistré")}</strong> — {t("connectez-vous avec lui en jeu, l'addon installé, puis faites /reload, déconnectez-vous ou quittez le jeu : le jeu n'écrit les données de l'addon qu'à ce moment-là.")}
            </li>
          </ul>
        </section>
      ) : (
        <div className="character-cards">
          {characters.map((c) => {
            const p = c.profile;
            const [first, realm] = p.name.split("-");
            const guide = guides.find((g) => g.id === assignments[p.name]);
            const version = guide && followed(guide);
            // Those of its race and class (and the one it follows), or all of them, its own first.
            const fits = (g: GuideSummary) => latest(g).race === c.race && latest(g).class === c.class;
            const others = guides.filter((g) => !fits(g) && (allGuides || g.id === guide?.id));
            const xp = p.xp_max > 0 ? Math.min(100, (100 * p.xp) / p.xp_max) : 0;
            return (
              <section className={`panel character-card${guide ? " has-guide" : ""}`} key={p.name}>
                <header className="character-head">
                  <FactionIcon faction={options?.races.find((r) => r.key === c.race)?.faction ?? (c.race ? raceFaction(c.race) : undefined)} />
                  <div className="character-name">
                    <strong>{c.full_name ?? first}</strong>
                    <small>{realm}</small>
                  </div>
                  <span className="character-level">{t("Niv.")} {p.level}</span>
                  <button className="icon danger character-forget" title={t("Retirer de la liste")} aria-label={t("Retirer de la liste")} onClick={() => forget(p.name, c.full_name ?? first)}>
                    <Icon name="delete" />
                  </button>
                </header>
                <div className="character-who">
                  <span>{raceName(c.race)}</span>
                  <ClassName klass={c.class} />
                  <span className="muted">{t("Enregistré le {date}", { date: date(p.time) })}</span>
                </div>
                <div className="xp-bar" title={t("Niveau {level} ({pct} %)", { level: p.level, pct: Math.floor(xp) })}>
                  <span style={{ width: `${xp}%` }} />
                </div>
                {c.professions.length > 0 && (
                  <div className="character-professions">
                    {c.professions.map(([key, skill]) => (
                      <span key={key} className="profession-chip">
                        {professionName(key)} <strong>{skill}</strong>
                      </span>
                    ))}
                  </div>
                )}
                <div className="character-guide">
                  <select aria-label={t("Guide")} value={guide?.id ?? ""} onChange={(e) => assign(p.name, e.target.value || null)}>
                    <option value="">{t("Aucun guide")}</option>
                    {[
                      [t("Même race et classe"), guides.filter(fits)],
                      [t("Autres guides"), others],
                    ].map(([label, list]) =>
                      (list as GuideSummary[]).length ? (
                        <optgroup key={label as string} label={label as string}>
                          {(list as GuideSummary[]).map((g) => (
                            <option key={g.id} value={g.id}>
                              {g.name}
                            </option>
                          ))}
                        </optgroup>
                      ) : null,
                    )}
                  </select>
                  <div className="character-guide-actions">
                    {guide && version && <button onClick={() => onOpenGuide(guide.id, version.version)}>{t("Voir le guide")}</button>}
                    <button className={guide ? "" : "primary"} onClick={() => onNewGuide(p.name)}>
                      {t("Créer un guide")}
                    </button>
                  </div>
                  {guide && version && (
                    <span className="muted character-guide-meta">
                      {t("Niv. {from}–{to} · {time}", { from: version.from_level, to: version.to_level, time: version.total_time_text })}
                    </span>
                  )}
                </div>
              </section>
            );
          })}
        </div>
      )}
    </div>
  );
}
