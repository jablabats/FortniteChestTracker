using System.Text.Json;
using ChestCounter;

var output = Console.Out;
Console.SetOut(TextWriter.Null);
try
{
    L.Set("en");
    using var replay = File.OpenRead(args.Single());
    var match = new ReplayAnalyzer().Analyze(replay, args[0], null);
    if (!match.IsBattleRoyale || !match.IsReplayOwner)
        throw new InvalidDataException("The replay did not identify a Battle Royale recording player.");

    var treasures = match.Chests.Select(chest => new Entry(
        chest.TimeSeconds, true, L.Name(chest.Kind) + (chest.Destroyed ? " (broken)" : ""),
        "You", chest.Items.Select(item => new Loot(TreasureNames.Friendly(item))).ToArray()
    )).ToList();
    void AddPrizes(IEnumerable<TreasureFind> finds, string opener)
    {
        foreach (var find in finds)
        {
            var loot = new Loot(find.DisplayName);
            var source = L.Name(find.SourceKind);
            if (find.SourceDestroyed) source += " (broken)";
            var chest = opener == "You" && find.SourceCertain && find.SourceIsChest && find.ObtainedAtSeconds is float time
                ? treasures.FirstOrDefault(entry => entry.seconds is float seconds && Math.Abs(seconds - time) <= 1
                    && entry.source == source)
                : null;
            if (chest is not null)
            {
                if (!chest.loot.Contains(loot))
                    treasures[treasures.IndexOf(chest)] = chest with { loot = [.. chest.loot, loot] };
            }
            else
            {
                treasures.Add(new Entry(find.ObtainedAtSeconds, find.TimeExact,
                    find.SourceCertain ? source : source + " (unconfirmed)", opener, [loot]));
            }
        }
    }
    AddPrizes(match.TreasureFinds, "You");
    AddPrizes(match.TeamTreasureFinds, "Teammate");
    output.Write(JsonSerializer.Serialize(new
    {
        verified_chests = checked(match.ChestsOpened + match.AmmoBoxesOpened),
        ammo_crates = match.AmmoBoxesOpened,
        treasures = treasures.OrderBy(entry => entry.seconds ?? float.MaxValue)
    }));
}
catch (Exception error)
{
    output.Write(JsonSerializer.Serialize(new { error = error.Message }));
}

record Entry(float? seconds, bool opening_observed, string source, string opener, Loot[] loot)
{
    public float[]? position => null;
}

record Loot(string item)
{
    public uint? count => null;
    public string? picked_up_by => null;
}
