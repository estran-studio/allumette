# Recette du transport WebRTC

Ce test ouvre deux navigateurs isolés, obtient des credentials TURN temporaires
auprès d’Allumette, force `iceTransportPolicy: relay` et vérifie un échange réel
dans les deux sens. Il vérifie aussi que les candidats sélectionnés sont de type
`relay`. Il ne teste pas le signaling Matchbox, les jeux ou deux réseaux distincts.
Aucun secret du VPS n’est nécessaire ni affiché.

```sh
cd tools/webrtc-smoke
npm ci --ignore-scripts
ALLUMETTE_ORIGIN=https://allumette.estran.studio CHROMIUM_EXECUTABLE=/usr/bin/chromium npm test
TURN_TRANSPORT=udp npm test
TURN_TRANSPORT=tcp npm test
```

`CHROMIUM_EXECUTABLE` doit désigner un Chromium installé. Sur un environnement
sans sandbox utilisable, `CHROMIUM_NO_SANDBOX=1` active explicitement ce mode pour
la recette. Les origines/pages de production ne sont pas modifiées.

Recette du 2026-10-09 : Chromium 154 sur Debian 13 passe avec la configuration
complète, UDP seul et TCP seul sur le VPS Allumette. Les deux côtés sélectionnent
un relais et échangent plus de 2 KiB, incluant le message et son acquittement.
Les deux navigateurs utilisent le même hôte Linux : cette preuve ne remplace
pas le test depuis deux réseaux d’amis.

Sur le Mac de recette, même un test direct sans engine ni serveur n’établit pas
le canal. La route vers le VPS passe par `utun4`; des requêtes STUN/TURN expirent.
Le réseau local/VPN est une piste à vérifier en comparant le même test hors tunnel.
Les allocations TURN seules ne démontrent pas que le canal fonctionne.
