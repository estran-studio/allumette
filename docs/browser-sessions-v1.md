# Contrat de sessions navigateur multijeu — v1

2026-10-09 · Base étudiée `b6d0178`. Les extensions ci-dessous sont un contrat de réalisation,
à livrer de façon compatible avec les clients actuels. Allumette est indépendante de tout moteur.

Mise à jour : une première implémentation opt-in existe dans `src/browser.rs`, sous
`/browser/v1`. Le SDK sans dépendance moteur/UI est dans `packages/browser-client`
(version 0.1.0). Les endpoints historiques restent disponibles. Voir la section
« Implémentation actuelle » en fin de document pour les différences avec la cible.

## Responsabilités

Allumette possède identités, salons, invitations, présence/readiness, sessions, signaling et
configuration du relais coturn. Le jeu possède simulation, inputs, règles, assets et seed.
Le serveur ne dépend pas de GGRS, Bevy, RON ou Alacod. `game_id` et `compatibility_id` sont
opaques : deux applications peuvent utiliser des formats de contenu et des moteurs différents.
Le SDK JS ne charge aucun jeu. Le frontend hôte applique le contrat pour lancer son propre client.

## API actuelle à conserver

`POST /auth/challenge`, `POST /auth/login`, `GET/POST /lobbies`,
`GET /lobbies/stream` (SSE), `POST /lobbies/:id/join`, `POST /lobbies/:id/start`,
`DELETE /lobbies/:id`, `POST /lobbies/:id/invite` (whitelist de clés), `GET /ice-servers`,
`GET /health` et WebSocket `/<JWT>` (protocole Matchbox).
Le JWT actuel identifie un utilisateur; le salon est retrouvé dans la table des adhésions.
Le SDK doit distinguer un JWT d'identité, un droit d'invitation et un ticket de signaling.

## Extensions proposées

Les nouveaux champs sont opt-in pour les clients historiques. Un salon v1 exige un contrat
complet; la compatibilité de l'ancien mode sera explicitement dépréciée après migration.

Création : `{ game_id, compatibility_id, capacity: 2|4, is_private: true }`.
Le serveur applique les limites par jeu/configuration, vérifie les champs non vides et fixe
la compatibilité à la création. Un membre annonce son `compatibility_id` lors de l'adhésion.
Capacité maximale, readiness et l'ensemble des membres sont vérifiés atomiquement.

DTO salon : `{ id, game_id, compatibility_id, capacity, status, is_owner, players,
 session_id?, revision }`. Les joueurs incluent identité stable, pseudonyme et prêt, uniquement
pour les membres autorisés. L'ordre canonique est lexicographique par identité; il n'est pas
la correspondance de handles du moteur. Aucun ordre ne vient d'un HashSet non trié.

Invitation : proposer `POST /lobbies/:id/invitations` (propriétaire) et
`POST /lobbies/:id/join` avec `{ invitation_token, compatibility_id }`.
Le token est aléatoire, stocké sous empreinte, limité à un salon, expire (15 min initialement),
révocable et soumis à la capacité. Il autorise l'identité authentifiée à rejoindre sans échange
manuel de clés. Le lien public appartient au frontend; Allumette retourne la capacité opaque,
sans imposer un domaine de jeu. Le token ne figure pas dans les DTO publics ni les logs.

Readiness : proposer `POST /lobbies/:id/ready` avec `{ ready, compatibility_id }`.
Le propriétaire démarre via `/start` seulement lorsque les places attendues sont remplies et
prêtes. Cette transition crée un `session_id` nouveau et fige les participants. La connexion du
propriétaire au WebSocket ne démarre plus implicitement un salon v1.

Contexte de session : proposer `GET /lobbies/:id/session` pour les membres :
`{ schema_version: 1, session_id, game_id, compatibility_id, capacity, participants,
 local_player_id, signaling_url, ice_servers, expires_at }`.
Le ticket court de `signaling_url` lie identité/salon/session et ne s'utilise que dans cette
session. Le serveur refuse les connexions obsolètes et doubles connexions non autorisées.
La correspondance identité ↔ PeerId est fournie par un événement de session/SSE ou le signaling
versionné, et le SDK la rend disponible au jeu avant l'association des joueurs.

Chaque requête conserve les erreurs HTTP lisibles : 401 auth expirée, 403 droits, 404 absent,
409 capacité/session/compatibilité, 400 contrat invalide. Définir des codes JSON stables pour les
nouveaux clients. Le client ne transforme pas un 409 de compatibilité en simple retry.

## Machine d'état

`Waiting → Connecting → InProgress → Waiting`, avec identifiant distinct pour chaque partie.
Un départ en Waiting libère la place et invalide readiness; propriétaire parti ⇒ salon supprimé.
Démarrage validé ⇒ participants figés; `Connecting` borné (30 s initialement). Un échec ferme la
session et revient au salon, sans rejoindre en cours. Fin : tous les pairs quittent la session ou
un délai borné l'expire; les membres du salon restent disponibles pour une nouvelle partie.
Le jeu sait annoncer la fin sans se connecter à une salle dérivée du JWT. Une session terminée
n'accepte jamais de nouveaux signaux. L'isolation des signaux vérifie salon **et** session.

## ICE et SDK

`/ice-servers` émet déjà STUN puis TURN avec username temporel + UUID et HMAC-SHA1 Base64.
Le secret durable reste entre Allumette et coturn. Le client doit utiliser les entrées TURN
et leurs credentials, même si son builder ne prend qu'une configuration ICE : l'adaptateur
doit traiter cette limitation explicitement. Test obligatoire en mode `relay`.
Le repli STUN devient un résultat de dégradation explicite, pas une réussite silencieuse.

SDK : configuration du domaine avant restauration de session; `loginGuest(username)` local,
`createSessionLobby`, `acceptInvitation`, `setReady`, `getSessionContext`, `leaveSession`.
Les événements sont indépendants du moteur; le frontend décide quoi faire avec le contexte.
Une identité de première session peut être temporaire et sa récupération optionnelle.
Pas de wallet obligatoire. Un JWT expiré repasse par le challenge; une identity perdue demande
une nouvelle admission. La migration de domaine ne transfère pas automatiquement localStorage.

SSE : snapshot initial, reconnect sur EOF et erreur, abort propre, refresh après reconnect,
contrôle expiration auth et backoff borné. Déconnecter le stream lorsqu'aucune page en ligne
n'en a besoin; le solo ne dépend pas du SDK ou d'un serveur vivant.

## Livraison et preuve

Ordre : tests/contrat serveur → SDK versionné → adaptateurs hôtes (Alacod d'abord).
Versionner serveur et SDK séparément; garder les images et dépendances verrouillées.
Un serveur unique conserve pour l'instant ses salons en RAM; documenter leur perte au restart.
Le proxy transporte HTTP/SSE/WSS; coturn est exposé directement (DNS-only), quotas et TTL fixés.
En production, secrets explicites, CORS/origines configurées, tickets masqués dans les logs.

Tests nécessaires : deux IDs de jeu et plusieurs salons isolés; dépassement de capacité sous
joins concurrents; invitation expirée/révoquée; incompatible refusé; readiness et owner start;
tickets périmés; association des peers; suppression et double onglet; deux sessions successives;
SSE EOF/reconnect; TURN relay forcé avec deux navigateurs sur réseaux distincts.
Les tests de protocole utilisent des clients fixtures, sans dépendance à un moteur.

## Implémentation actuelle — première tranche navigateur

Les salons `/browser/v1` sont toujours privés et séparés des salons historiques.
Une identité ne peut appartenir simultanément à un salon historique et navigateur.
Les capacity acceptées sont 2 et 4; tous les membres doivent être prêts avant start.
Le DTO utilise `player_id` pour les membres et `peer_id` dans le contexte de session.
Les joins concurrents sont sérialisés par un verrou unique sur cet état.

Une nouvelle invitation remplace la précédente; durée 15 minutes. Les invitations
sont stockées sous SHA-256 et ne sont pas présentes dans les DTO. Le frontend construit
son lien. Une session expire après 30 minutes, avec 120 secondes pour le signaling
initial (les builds navigateur actuels sont volumineux). Une session a son propre UUID.
Le contexte expose `signaling_path`; le SDK le résout depuis son origine explicite
et ajoute `signaling_url` et `ice_servers` via `/ice-servers`. Le ticket HS256 porte
l’audience `allumette-browser-signaling` et expire à 60 secondes. Les endpoints
HTTP ne l’acceptent pas comme JWT d’identité. Le signaling garde le format Matchbox.

`InProgress` indique que les sockets de signaling attendus ont été attribués;
le jeu hôte doit vérifier séparément WebRTC et le démarrage de sa simulation.
Le dernier socket fermé ramène au salon et annule readiness. `DELETE /session`
permet à un membre de terminer la session pour tous. Quitter un salon en cours
termine également la session; propriétaire parti ⇒ salon supprimé.
Le serveur ne reçoit ni seed, ni paramètres de simulation, ni assets.

Le nouveau SDK utilise un guest éphémère WebCrypto Ed25519 et un polling contrôlé
par l’hôte. Il ne persiste ni JWT ni clé privée. Le SSE existant appartient au SDK
historique : le nouveau contrat ne dépend pas de ce stream. Les salons disparaissent
après 15 minutes d’inactivité en Waiting ou au redémarrage du processus.

Tests : compatibilité, readiness, capacité avec joins concurrents, rotation/expiration
d’invitations, tickets et sessions périmées, isolation multijeu et messages Matchbox.
La validation TURN sur réseaux distincts reste un critère de livraison.
`ALLOWED_ORIGINS` configure une liste explicite d’origines CORS; le VPS de recette
l’utilise. Sans cette variable, le comportement historique permissif est conservé
pour compatibilité. Ne pas confondre les allocations TURN vérifiées avec une
preuve de data channel ou une ouverture publique.
