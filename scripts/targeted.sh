#!/usr/bin/env bash
# Records and replays the batches built around particular effects: combinations
# that random teams almost never produce, each of which either found a bug or
# is needed to catch an injected one (see scripts/mutation_test.py).
#
#   scripts/targeted.sh [dir]              record every batch into dir (default: a temp dir) and replay it
#   ONLY=darts scripts/targeted.sh [dir]   only the batches whose name contains "darts"
#   JOBS=4 scripts/targeted.sh [dir]       record that many batches at a time (default 2)
#   SHOWN=1 scripts/targeted.sh [dir]      also record Showdown's log, with open team sheets, and check what
#                                          each side has been shown against it, with the sheets open and as
#                                          if they had stayed closed (files are about fifty times larger)
#   LOGS=1 scripts/targeted.sh [dir]       also record Showdown's log and requests, with open team sheets: the
#                                          battles are the same, and the files can be followed
#                                          (target/release/followcheck --export --require-state)
#   DIFFTEST_ARGS=--strict scripts/targeted.sh   pass arguments to every replay (--strict: an unsupported
#                                          battle fails too)
#   PS_DIR=/path/to/pokemon-showdown       a built Showdown other than oracle/pokemon-showdown (as lib.js takes it)
#
# Keep the directory to hand the batches to the mutation test:
#   scripts/targeted.sh corpus && scripts/mutation_test.py corpus/*.jsonl
set -euo pipefail
cd "$(dirname "$0")/.."

if [ ! -f "${PS_DIR:-oracle/pokemon-showdown}/dist/sim/index.js" ]; then
  echo "Showdown is not built yet; run scripts/setup-oracle.sh first (or set PS_DIR to a built one)." >&2
  exit 1
fi
OUT="${1:-}"
if [ -z "$OUT" ]; then
  OUT="$(mktemp -d)"
  trap 'rm -rf "$OUT"' EXIT
fi
mkdir -p "$OUT"
cargo build --quiet --release --bin difftest

# name | battles | seed | gen_cases.js arguments
batches() {
  cat <<'EOF'
substitute|500|6800|--moves substitute,bulletseed,rockblast,icywind,fakeout,nuzzle,gigadrain,doubleedge,hypervoice,dragoncheer,focusenergy --abilities parentalbond,sheerforce,skilllink,roughskin,static,infiltrator,intimidate,noability --items lifeorb,rockyhelmet,airballoon,occaberry,chilanberry,shellbell,kingsrock
move-locks|500|6801|--moves gastroacid,encore,disable,taunt,imprison,torment,fakeout,helpinghand,followme,ragepowder --items mentalherb,quickclaw,choicescarf,sitrusberry --mega-rate 1 --check-legal
trapping|500|6802|--moves perishsong,destinybond,leechseed,ingrain,meanlook,block,jawlock,spiritshackle,yawn,octolock,wrap,firespin,stockpile,spitup,swallow,charge --abilities liquidooze,magicguard,soundproof,shadowtag,insomnia,sweetveil,electromorphosis,noability --items bigroot,bindingband,shedshell,leftovers
screens|300|6810|--moves auroraveil,reflect,lightscreen,brickbreak,psychicfangs,snowscape --abilities snowwarning,infiltrator,noability --items lightclay
hazard-removal|300|6811|--moves rapidspin,mortalspin,spikes,stealthrock,toxicspikes,leechseed,wrap --abilities sheerforce,noability
unaware|300|6812|--moves doubleteam,minimize,swordsdance,irondefense,calmmind --abilities unaware,noability
leek|300|6813|--species farfetchd --items leek --moves focusenergy,leafblade,nightslash
stolen-mega-stone|300|6814|--species floetteeternal,florges --items floettite --moves trick,knockoff,thief,covet,switcheroo
sparkling-aria|300|6815|--species primarina --moves sparklingaria,willowisp,scald,protect --abilities sheerforce,noability
reckless|300|6816|--abilities reckless,noability --moves highjumpkick,doubleedge,bravebird,flareblitz,protect
sheer-force-electro-shot|300|6817|--species archaludon --abilities sheerforce,noability --moves electroshot,meteorbeam,raindance --items kingsrock
vetoed-moves|300|6818|--moves throatchop,hypervoice,roar,partingshot,perishsong,snarl --abilities magicbounce,soundproof,noability
ally-moves|150|9801|--moves pollenpuff,shellsidearm,beatup,curse,metalburst,comeuppance,substitute,psychicnoise,trickortreat,soak,doubleedge,followme
shell-side-arm-tie|400|9961|--species slowbrogalar --moves shellsidearm,swordsdance,bulkup,screech,faketears,metalsound,charm
dragon-darts|150|9802|--moves dragondarts,protect,followme,ragepowder,substitute,detect,spikyshield,phantomforce,fly --species dragapult,clefable,sylveon,gardevoir,azumarill,skarmory,corviknight
queue-order|150|9803|--moves afteryou,quash,round,instruct,copycat,sleeptalk,rest,sleeppowder,yawn,counter,mirrorcoat,hypervoice
called-moves|150|9804|--moves copycat,metalburst,fly,hyperbeam,outrage,explosion,rest,sleeptalk,substitute,protect,followme,dragondarts,bulletseed,snore,round --abilities pressure,moldbreaker,protean,prankster,magicbounce,noability --items metronome,choicescarf,quickclaw,chestoberry,leppaberry
future-sight|150|9805|--moves futuresight,protect,substitute,fly,wish,copycat,metalburst,psychic,explosion
fling|150|9806|--moves fling,protect,substitute,taunt,encore,swordsdance,knockoff,recycle --items kingsrock,lightball,poisonbarb,mentalherb,whiteherb,sitrusberry,lumberry,leppaberry,occaberry,leek,normalgem,lifeorb,rockyhelmet
ally-switch|150|9807|--moves allyswitch,futuresight,wish,leechseed,followme,fakeout,earthquake,helpinghand,pollenpuff,healpulse,instruct,afteryou,dragondarts,snipeshot,curse,substitute
pivoting|150|9808|--moves uturn,voltswitch,flipturn,partingshot,chillyreception,protect,substitute,stealthrock,spikes,fakeout
forced-switches|150|9809|--moves roar,whirlwind,dragontail,circlethrow,protect,substitute,stealthrock,spikes,toxicspikes,stickyweb,ingrain,uturn --abilities suctioncups,guarddog,magicbounce,moldbreaker,intimidate,regenerator,naturalcure,noability,soundproof
switch-items|150|9810|--moves uturn,voltswitch,dragontail,doubleedge,highjumpkick,fakeout,earthquake,rockslide,bulletseed,stealthrock,dragondarts,knockoff --abilities emergencyexit,roughskin,pickpocket,magician,sheerforce,noability --items ejectbutton,redcard,lifeorb,rockyhelmet,shellbell,sitrusberry
baton-pass|150|9811|--moves batonpass,shedtail,swordsdance,substitute,leechseed,confuseray,perishsong,powertrick,gastroacid,ingrain,aquaring,focusenergy,meanlook,curse,stockpile,magnetrise
healing-wish|150|9812|--moves healingwish,allyswitch,uturn,willowisp,toxic,thunderwave,protect,wish,batonpass,spikes
revival-blessing|150|9813|--moves revivalblessing,explosion,healingwish,uturn,doubleedge,closecombat,protect,stealthrock --species pawmot
formes|150|9814|--species aegislash,morpeko,mimikyu,palafin,greninja --abilities stancechange,hungerswitch,disguise,zerotohero,battlebond,iceface,gulpmissile,shieldsdown,terashell,moldbreaker,noability --moves kingsshield,aurawheel,uturn,flipturn,shadowsneak,surf,snowscape,confuseray,substitute,swordsdance,roar
illusion-transform|150|9815|--species zoroark,zoroarkhisui,ditto --abilities illusion,imposter,intimidate,noability,psychicsurge,drizzle,regenerator --moves transform,substitute,uturn,roost,dragoncheer,focusenergy,swordsdance,powertrick,trickortreat,soak,gastroacid,worryseed,skillswap
two-turn-moves|150|9816|--moves outrage,thrash,petaldance,hyperbeam,gigaimpact,fly,dig,dive,bounce,solarbeam,meteorbeam,electroshot,uproar,focuspunch,beakblast,counter,mirrorcoat --abilities sheerforce,forewarn,anticipation,noability
item-tampering|150|9817|--moves knockoff,thief,covet,trick,switcheroo,bugbite,pluck,recycle,poltergeist,belch,stuffcheeks,teatime,skillswap,entrainment,roleplay,simplebeam,worryseed --abilities cudchew,magician,pickpocket,stickyhold,unburden,noability --items sitrusberry,lumberry,leppaberry,kingsrock,mentalherb
drag-out-recheck|300|9901|--moves dragontail,circlethrow,protect --abilities wanderingspirit,suctioncups,noability
grav-apple|200|9902|--moves gravapple,gravity,protect --species flapple,clefable,gardevoir
pollen-puff-heal-block|300|9903|--moves pollenpuff,psychicnoise
darts-into-immunity|400|9904|--moves dragondarts,protect,detect --species dragapult,clefable,sylveon,gardevoir,azumarill,garchomp,tyranitar --abilities berserk,emergencyexit,noability
sleep-talk-charging|300|9905|--moves rest,sleeptalk,solarbeam,fly,dig,bounce,protect
baton-pass-copy|300|9906|--moves gastroacid,batonpass,powertrick,protect --species medicham,arbok,snorlax,serperior,clefable,ninetales,vaporeon,jolteon,aegislash,mimikyu --abilities stancechange,disguise,intimidate,noability
clangorous-soul-maxed|500|9907|--species kommoo,clefable,gardevoir,audino --moves clangoroussoul,rest,healpulse,protect --abilities simple,noability --items leftovers,sitrusberry,chestoberry
disguise-resist-berry|300|9908|--species mimikyu,gengar,aegislash,scizor --abilities disguise,noability --moves shadowball,shadowsneak,ironhead,flashcannon,shadowclaw,bulletpunch,protect --items kasibberry,babiriberry
transform-into-formes|300|9909|--species ditto,mimikyu,morpeko,aegislash,palafin --abilities imposter,disguise,hungerswitch,stancechange,zerotohero --moves transform,shadowsneak,aurawheel,kingsshield,protect,flipturn,playrough,shadowclaw,ironhead
darts-into-protect|800|9910|--species dragapult,snorlax,slowbro,hippowdon --abilities berserk,emergencyexit --moves dragondarts,protect
sleep-talk-meteor-beam|300|9912|--moves-per 4 --species archaludon,garganacl,glimmora,aggron,tyrantrum --moves rest,sleeptalk,electroshot,meteorbeam
electrified-struggle|600|9914|--moves-per 4 --species heliolisk,garchomp,hippowdon,excadrill --moves electrify,taunt,swordsdance,calmmind,nastyplot,bulkup,irondefense,amnesia
helping-hand-twice|500|9920|--species oranguru --moves helpinghand,instruct,rockslide --moves-per 3
magnet-rise-grounded|300|9921|--moves magnetrise,smackdown,ingrain --moves-per 3
smack-down-twice|500|9922|--moves smackdown,fly,bounce --moves-per 3
metal-burst-substitute|600|9935|--species sableye,aggron,bastiodon,rhyperior,perrserker,orthworm,kingambit,houndoom,pangoro,mabosstiff --moves metalburst,comeuppance,substitute,nuzzle,fakeout,icywind,bulletseed --moves-per 4
future-sight-own-slot|400|9925|--species alakazam,mrmime,chimecho,musharna,reuniclus,farigiraf --moves futuresight,allyswitch
oblivious-taunt|300|9926|--moves taunt,skillswap,attract --abilities oblivious,moldbreaker,noability
armor-tail-field-moves|300|9927|--abilities prankster,armortail,queenlymajesty,noability --moves raindance,sunnyday,haze,grassyterrain,perishsong,trickroom
gastro-acid-ability-end|400|9928|--moves gastroacid,skillswap,worryseed,flamethrower,heatwave,willowisp --abilities flashfire,illusion,unburden,noability --items sitrusberry
merciless|300|9929|--abilities merciless,noability --moves toxic,poisonjab,sludgebomb,toxicspikes
oblivious|300|9930|--abilities oblivious,cutecharm,moldbreaker,noability --moves taunt,attract,skillswap,fakeout,closecombat
damp-aftermath|300|9931|--abilities damp,aftermath --moves closecombat,doubleedge,uturn,explosion
leaf-guard-yawn|300|9932|--abilities leafguard,drought,noability --moves yawn,sunnyday,raindance
freeze|800|9933|--abilities magmaarmor,synchronize,moldbreaker,noability --items lumberry,aspearberry --moves icebeam,blizzard,freezedry,icepunch,triattack,skillswap
own-tempo|300|9934|--abilities owntempo,moldbreaker,noability --moves confuseray,swagger,dynamicpunch,skillswap,hurricane
said-aloud|400|9940|--abilities limber,insomnia,vitalspirit,immunity,waterbubble,thermalexchange,magmaarmor,purifyingsalt,leafguard,overcoat,moldbreaker,noability --moves thunderwave,sleeppowder,toxic,willowisp,glare,stunspore,poisonpowder,yawn,hypnosis,sing,sunnyday,icebeam --items lumberry
heal-bell-deaf|200|9941|--species chimecho --moves healbell,toxic,thunderwave --abilities soundproof,goodasgold,noability
gravity-grounds|200|9942|--moves gravity,fly,bounce,highjumpkick,magnetrise --moves-per 3
octolock-blocked|200|9943|--moves octolock,protect,icywind --abilities clearbody,whitesmoke,bigpecks,noability --moves-per 3
symbiosis-resist-berry|300|9944|--abilities symbiosis,noability --items yacheberry,chilanberry,occaberry,passhoberry,shucaberry,damprock --moves icebeam,doubleedge,flamethrower,surf,earthquake,protect
ability-swapped-mid-hit|300|9945|--abilities wanderingspirit,mummy,poisontouch,noability --moves doublehit,doubleedge,closecombat,uturn,protect --items pechaberry,lumberry
fling-eaten-first|200|9946|--moves fling,protect --abilities spicyspray,noability --items lumberry,rawstberry --moves-per 2
healing-wish-ally-switch|600|9949|--moves healingwish,allyswitch,fakeout --moves-per 3
borrowed-move-stopped|300|9951|--moves copycat,uproar,outrage,thrash,petaldance,disable,protect --abilities cursedbody,noability --moves-per 3
zoroark-disguises|400|7101|--species zoroark,zoroarkhisui --moves uturn,protect,nightdaze,flamethrower,knockoff,poisonjab,toxic,willowisp,explosion
zoroark-on-the-team|600|7103|--species zoroark,zoroarkhisui,garchomp,milotic,arcanine,sylveon,snorlax,scizor,dragapult,gengar,politoed,azumarill
no-retreat|400|9960|--species falinks,umbreon,snorlax,gengar --moves noretreat,meanlook,block,jawlock,batonpass,uturn,roar,copycat,instruct --moves-per 3 --check-legal
perish-trap|600|9950|--species gengar,umbreon,politoed,azumarill,slowbro,snorlax,dragapult,kommoo --moves perishsong,meanlook,block,protect,uturn,batonpass,roar,whirlwind --abilities shadowtag,soundproof,noability --items gengarite,shedshell,ejectbutton,redcard --check-legal
EOF
}

record() {
  IFS='|' read -r name n seed args <<<"$1"
  # shellcheck disable=SC2086
  node oracle/gen_cases.js --n "$n" --seed "$seed" $args ${SHOWN:+--log --open-sheets} ${LOGS:+--log --requests --open-sheets} \
    --out "$2/$name.jsonl" >"$2/$name.gen.log" 2>&1 ||
    { echo "$name: recording failed: $(grep -m 1 '^Error' "$2/$name.gen.log" || tail -n 1 "$2/$name.gen.log")" >&2; exit 255; }
}
export -f record

# (One batch per line, NUL-separated: the same on GNU and BSD xargs.)
export OUT
batches | grep -- "${ONLY:-}" | tr '\n' '\0' | xargs -0 -n 1 -P "${JOBS:-2}" bash -c 'record "$1" "$OUT"' _

status=0
while IFS='|' read -r name _; do
  # shellcheck disable=SC2086
  line="$(./target/release/difftest "$OUT/$name.jsonl" --quiet ${DIFFTEST_ARGS:-} | tail -n 1)" || status=1
  printf '%-26s %s\n' "$name" "$line"
  if [ -n "${SHOWN:-}" ]; then
    for sheets in "" --closed-sheets; do
      report="$(./target/release/difftest "$OUT/$name.jsonl" --shown $sheets --quiet)" || status=1
      printf '%-26s %s\n' "" "${report%%$'\n'*}"
    done
  fi
done < <(batches | grep -- "${ONLY:-}")
exit $status
