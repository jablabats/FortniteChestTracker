const fs = require("node:fs");

function finitePosition(point) {
  return point && [point.x, point.y, point.z].every(Number.isFinite)
    ? [point.x, point.y, point.z] : null;
}

const guid = value => typeof value === "number" ? value : value?.value;

function stateFor(map, key) {
  if (!map.has(key)) map.set(key, {});
  return map.get(key);
}

function getActorId(event) {
  return event.globalData.channelToActor[event.chIndex];
}

async function readReplay(path) {
  const parser = require("fortnite-replay-parser");
  const defaults = name => require(`fortnite-replay-parser/NetFieldExports/${name}.json`);
  const addProperties = (name, properties) => {
    const group = defaults(name);
    return { ...group, properties: { ...group.properties, ...properties } };
  };
  const classProperty = (name, type) => ({ name, type, parseType: "readClass" });

  const containers = new Map();
  const pickups = new Map();
  const players = new Map();
  const pawns = new Map();
  let recorder;

  const chestTypes = {
    Tiered_Chest_Athena: "Chest",
    Creative_Tiered_Chest: "Creative chest",
    Tiered_Chest_Athena_FactionChest_NoLocks: "Faction chest",
  };
  const chestGroup = defaults("Chests");
  const extraChestGroups = Object.keys(chestTypes).slice(1).map(name => ({
    ...chestGroup,
    path: [`/Game/Building/ActorBlueprints/Containers/${name}.${name}_C`],
    staticActorIds: [name],
  }));

  await parser(fs.readFileSync(path), {
    parseLevel: 2,
    parseEvents: false,
    useCheckpoints: false,
    customClasses: {
      ReplayTeam: class {
        serialize(reader) {
          const bits = reader.lastBit - reader.offset;
          if (bits > 0 && bits <= 16) this.value = reader.readBitsToUnsignedInt(bits);
        }
      },
    },
    customNetFieldExports: [
      addProperties("GameState", { RecorderPlayerState: classProperty("RecorderPlayerState", "ActorGuid") }),
      addProperties("FortPlayerState", { TeamIndex: classProperty("TeamIndex", "ReplayTeam") }),
      ...extraChestGroups,
    ],
    handleEventEmitter({ propertyExportEmitter: events }) {
      events.on("Athena_GameState.Athena_GameState_C", ({ data }) => {
        recorder ??= guid(data.RecorderPlayerState);
      });
      events.on("FortniteGame.FortPlayerStateAthena", event => {
        const id = getActorId(event);
        if (!id) return;
        const player = stateFor(players, id);
        player.team = guid(event.data.TeamIndex) ?? player.team;
      });
      events.on("PlayerPawn_Athena.PlayerPawn_Athena_C", event => {
        const id = getActorId(event);
        if (id && event.data.PlayerState) pawns.set(id, guid(event.data.PlayerState));
      });
      events.on("Chests", event => {
        const id = getActorId(event);
        if (!id) return;
        const { data, timeSeconds, globalData } = event;
        const wasSeen = containers.has(id);
        const chest = stateFor(containers, id);
        const actor = globalData.channels[event.chIndex]?.actor;
        const archetype = globalData.netGuidCache.tryGetPathName(actor?.archetype?.value) ?? "";
        const type = Object.keys(chestTypes).find(name => archetype.includes(`.${name}_C`));
        const isAmmo = archetype.includes("Tiered_Ammo") || String(event.staticActorId ?? "").includes("Tiered_Ammo");
        // Some replays use the static actor ID as the only available class hint.
        const source = type ?? Object.keys(chestTypes).find(name => String(event.staticActorId ?? "").includes(name));
        if (!source && !archetype.includes("Chest") && !isAmmo) return;
        chest.source = isAmmo ? "Ammo box" : chestTypes[source] ?? "Chest";
        chest.position = finitePosition(data.ReplicatedMovement?.location)
          ?? chest.position ?? finitePosition(actor?.location);
        if (guid(data.SearchingPawn) && !chest.opened) {
          chest.searcher = guid(data.SearchingPawn);
          chest.search_time = timeSeconds;
        }
        if (data.bAlreadySearched === true && !chest.opened) {
          chest.opened = true;
          chest.seconds = Number.isFinite(timeSeconds) ? timeSeconds : null;
          chest.opening_observed = wasSeen;
          chest.pawn = chest.seconds !== null && chest.seconds - chest.search_time <= 10 ? chest.searcher : null;
        }
      });
      events.on("FortniteGame.FortPickupAthena", event => {
        const id = getActorId(event);
        if (!id) return;
        const { data, timeSeconds } = event;
        const pickup = stateFor(pickups, id);
        pickup.seconds ??= timeSeconds;
        pickup.position ??= finitePosition(data.LootInitialPosition) ?? finitePosition(data.ReplicatedMovement?.location);
        pickup.item ??= data.ItemDefinition?.name;
        pickup.count ??= data.Count;
        pickup.from_container ||= data.bTossedFromContainer === true;
        pickup.target ??= guid(data.PickupTarget);
        pickup.picked_up ||= data.bPickedUp === true;
      });
    },
  });

  const opener = pawn => {
    const state = pawns.get(pawn);
    if (!state || !recorder) return "Unknown";
    if (state === recorder) return "You";
    const ourTeam = players.get(recorder)?.team;
    const team = players.get(state)?.team;
    return ourTeam > 0 && team > 0 ? (team === ourTeam ? "Teammate" : "Other player") : "Unknown";
  };
  const treasures = [...containers.values()]
    .filter(chest => chest.opened && chest.source)
    .map(chest => ({
      seconds: chest.seconds,
      opening_observed: chest.opening_observed,
      position: chest.position ?? null,
      inferred_position: false,
      source: chest.source,
      opener: opener(chest.pawn),
      loot: [],
    }));
  const timed = treasures.filter(chest => chest.seconds !== null && chest.opening_observed);
  const possibleDrops = new Map();
  for (const pickup of pickups.values()) {
    if (!pickup.from_container || !pickup.item || !pickup.position || !Number.isFinite(pickup.seconds)) continue;
    const nearby = timed.filter(chest => {
      const delay = Math.abs(chest.seconds - pickup.seconds);
      return chest.position
        ? delay <= 1.25 && distance(chest.position, pickup.position) <= 250
        : delay <= 0.15;
    });
    if (nearby.length === 1) {
      if (!possibleDrops.has(nearby[0])) possibleDrops.set(nearby[0], []);
      possibleDrops.get(nearby[0]).push(pickup);
    }
  }
  for (const [chest, drops] of possibleDrops) {
    if (!chest.position) {
      if (drops.length < 2 || drops.some(drop => distance(drop.position, drops[0].position) > 250)) continue;
      chest.position = drops[0].position;
      chest.inferred_position = true;
    }
    chest.loot = drops.map(pickup => ({
      item: pickup.item.split("/").at(-1).split(".")[0],
      count: pickup.count > 0 ? pickup.count : null,
      picked_up_by: pickup.picked_up ? opener(pickup.target) : null,
    }));
  }
  return {
    treasures: treasures.filter(chest => chest.opener !== "Other player")
      .sort((a, b) => (a.seconds ?? Infinity) - (b.seconds ?? Infinity)),
  };
}

function distance(a, b) {
  return Math.hypot(...a.map((value, index) => value - b[index]));
}

// The parser occasionally logs while decoding. Reserve stdout for the JSON reply.
console.log = console.warn = console.error = () => {};
readReplay(process.argv[2]).then(
  result => process.stdout.write(JSON.stringify(result)),
  error => process.stdout.write(JSON.stringify({ error: String(error.message) })),
);
