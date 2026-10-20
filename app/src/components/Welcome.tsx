import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api, type Edition, type FoundGame, type WowStatus } from "../api";
import { useT } from "../i18n";
import { CLIENTS, SUPPORTED } from "../wow";

/** The game version of each game version folder the app has guides for. */
const EDITION_OF: Record<string, Edition> = { _forever_: "forever", _classic_beta_: "forever", _classic_era_: "classic", _anniversary_: "tbc" };

interface Props {
  /** Switch the game version the app works on. */
  onEdition: (edition: Edition) => Promise<void>;
  /** `addon`: open the addon's page (to install it). */
  onDone: (addon: boolean) => void;
}

/**
 * The first launch when World of Warcraft was not found: where it is (the folders found on this
 * computer, or one chosen), which game version, then on to the addon. Skipped, the red bar over
 * the pages still says what is missing.
 */
export function Welcome({ onEdition, onDone }: Props) {
  const t = useT();
  const [step, setStep] = useState(0);
  const [found, setFound] = useState<FoundGame[] | null>(null);
  const [dir, setDir] = useState("");
  const [status, setStatus] = useState<WowStatus | null>(null);
  const [edition, setEdition] = useState<Edition | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .detectWow()
      .then((list) => {
        setFound(list);
        if (list.length) setDir(list[0].root);
      })
      .catch(() => setFound([]));
  }, []);
  useEffect(() => {
    if (!dir.trim()) return;
    api
      .wowStatus(dir)
      .then(setStatus)
      .catch(() => setStatus(null));
  }, [dir]);

  // One folder per game version, the preferred one when a version has two (_forever_ and
  // _classic_beta_); the folders of a version are told on its card.
  const clients = status?.clients ?? [];
  const editions = SUPPORTED.filter((c, i) => clients.includes(c) && !SUPPORTED.slice(0, i).some((p) => clients.includes(p) && EDITION_OF[p] === EDITION_OF[c]));
  const foldersOf = (e: Edition) => SUPPORTED.filter((c) => clients.includes(c) && EDITION_OF[c] === e).join(", ");
  const usable = editions.length > 0;
  const finishFolder = async () => {
    setError(null);
    try {
      const settings = await api.getSettings();
      await api.setSettings({ ...settings, wow_dir: dir, welcome_done: true });
      const keep = editions.map((c) => EDITION_OF[c]).includes(settings.edition);
      setEdition(keep ? settings.edition : EDITION_OF[editions[0]]);
      setStep(1);
    } catch (e) {
      setError(String(e));
    }
  };
  const finishEdition = async () => {
    setError(null);
    try {
      if (edition) await onEdition(edition);
      setStep(2);
    } catch (e) {
      setError(String(e));
    }
  };
  const skip = async () => {
    const settings = await api.getSettings().catch(() => null);
    if (settings) await api.setSettings({ ...settings, welcome_done: true }).catch(() => {});
    onDone(false);
  };

  const card = (key: string, active: boolean, title: string, detail: string, onClick: () => void) => (
    <button key={key} role="radio" aria-checked={active} className={`setup-choice${active ? " active" : ""}`} onClick={onClick}>
      <strong>{title}</strong>
      <small>{detail}</small>
    </button>
  );

  return (
    <div className="welcome-overlay">
      <div className="panel welcome" role="dialog" aria-modal="true" aria-labelledby="welcome-title">
        <h2 id="welcome-title">{t("Bienvenue dans Factoruide")}</h2>
        {step === 0 && (
          <>
            <p>{t("Pour installer l'addon dans le jeu, l'application doit savoir où se trouve World of Warcraft.")}</p>
            {found === null ? (
              <p className="muted">{t("Recherche de World of Warcraft…")}</p>
            ) : found.length ? (
              <div className="setup-choices" role="radiogroup" aria-label={t("Dossiers trouvés")}>
                {found.map((f) => card(f.root, dir === f.root, f.root, f.clients.map((c) => CLIENTS[c] ?? c).join(", "), () => setDir(f.root)))}
              </div>
            ) : (
              <p className="muted">{t("World of Warcraft n'a pas été trouvé aux endroits habituels : choisissez son dossier.")}</p>
            )}
            <label className="field" htmlFor="welcome-dir">
              <span>{found?.length ? t("Ou un autre dossier") : t("Dossier World of Warcraft")}</span>
            </label>
            <div className="row">
              <input id="welcome-dir" value={dir} placeholder="C:\Program Files (x86)\World of Warcraft" onChange={(e) => setDir(e.target.value)} />
              <button
                onClick={async () => {
                  const picked = await open({ directory: true, defaultPath: dir || undefined, title: t("Dossier World of Warcraft") }).catch(() => null);
                  if (typeof picked === "string") setDir(picked);
                }}
              >
                {t("Parcourir…")}
              </button>
            </div>
            {status && dir.trim() && (
              <p className={usable ? "success" : "field-error"}>
                {usable
                  ? t("Trouvé : {list}.", { list: editions.map((c) => CLIENTS[c] ?? c).join(", ") }) + (status.root !== dir ? " " + t("Le dossier du jeu est {dir} : c'est lui qui sera enregistré.", { dir: status.root }) : "")
                  : status.problem === "missing"
                    ? t("Ce dossier n'existe pas.")
                    : t("Aucune version prise en charge dans ce dossier : choisissez celui qui contient _classic_era_, _anniversary_ ou _classic_beta_.")}
              </p>
            )}
          </>
        )}
        {step === 1 && (
          <>
            <p>{t("Sur quelle version du jeu jouez-vous ? Vous pourrez en changer à tout moment en haut du menu.")}</p>
            <div className="setup-choices" role="radiogroup" aria-label={t("Version du jeu")}>
              {editions.map((c) => card(c, edition === EDITION_OF[c], CLIENTS[c] ?? c, foldersOf(EDITION_OF[c]), () => setEdition(EDITION_OF[c])))}
            </div>
          </>
        )}
        {step === 2 && (
          <>
            <p>{t("C'est prêt. Installez l'addon Factoruide depuis CurseForge (tout se règle dans les options du jeu : /fg), puis créez un guide pour votre personnage.")}</p>
          </>
        )}
        {error && <div className="error">{error}</div>}
        <div className="setup-actions">
          {step < 2 ? (
            <button className="link" onClick={skip}>
              {t("Passer")}
            </button>
          ) : (
            <button className="link" onClick={() => onDone(false)}>
              {t("Plus tard")}
            </button>
          )}
          <span />
          {step === 0 && (
            <button className="primary" disabled={!usable} onClick={finishFolder}>
              {t("Suivant")}
            </button>
          )}
          {step === 1 && (
            <button className="primary" disabled={!edition} onClick={finishEdition}>
              {t("Suivant")}
            </button>
          )}
          {step === 2 && (
            <button className="primary" onClick={() => onDone(true)}>
              {t("Page de l'addon")}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
