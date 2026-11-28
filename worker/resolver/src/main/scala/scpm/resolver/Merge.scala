package scpm.resolver

import scala.collection.mutable

/**
 * The canonical merge of per-system classpaths (design §5.4, "Canonical merge"), so that every
 * machine writes the same lock bytes:
 *
 *   - entries are identified by (group, artifact, version, classifier, extension);
 *   - a Kahn topological sort over the union of the per-system orders, with an edge from each
 *     entry to the next one in every system's list;
 *   - among the ready entries, the one earliest in the first system (sorted) goes first, entries
 *     absent there after those present, then the smallest key.
 *
 * Each system's projection of the result is then exactly its own order. When the union has a
 * cycle, there is no such list, and the result is the per-system lists (`bySystem`).
 */
object Merge:
  /** The lock key of an entry, `group:artifact:version[:classifier][@extension]`. */
  def key(e: Entry): String =
    val classifier = e.classifier.filter(_.nonEmpty).fold("")(":" + _)
    val ext = if e.extension == "jar" then "" else "@" + e.extension
    s"${e.group}:${e.artifact}:${e.version}$classifier$ext"

  /** `Right(merged)` with `systems` on entries not on every system, or `Left(bySystem)`. */
  def merge(perSystem: Map[String, List[Entry]]): Either[Map[String, List[Entry]], List[Entry]] =
    val systems = perSystem.keys.toList.sorted
    val lists = systems.map(s => s -> distinct(perSystem(s))).toMap
    val entryOf = mutable.LinkedHashMap.empty[String, Entry]
    val presence = mutable.Map.empty[String, List[String]]
    val successors = mutable.Map.empty[String, mutable.Set[String]]
    val indegree = mutable.Map.empty[String, Int]
    for s <- systems; list = lists(s) do
      for e <- list do
        val k = key(e)
        entryOf.getOrElseUpdate(k, e)
        presence(k) = presence.getOrElse(k, Nil) :+ s
        indegree.getOrElseUpdate(k, 0)
      for (a, b) <- list.map(key).zip(list.map(key).drop(1)) do
        if successors.getOrElseUpdate(a, mutable.Set.empty).add(b) then indegree(b) = indegree(b) + 1
    val firstIndex: Map[String, Int] = systems.headOption.fold(Map.empty)(s => lists(s).map(key).zipWithIndex.toMap)
    val readyOrder: Ordering[String] = Ordering.by[String, (Int, String)](k => (firstIndex.getOrElse(k, Int.MaxValue), k))
    val ready = mutable.TreeSet.empty[String](using readyOrder)
    ready ++= indegree.collect { case (k, 0) => k }
    val out = mutable.ListBuffer.empty[String]
    while ready.nonEmpty do
      val next = ready.head
      ready -= next
      out += next
      for s <- successors.getOrElse(next, mutable.Set.empty) do
        indegree(s) = indegree(s) - 1
        if indegree(s) == 0 then ready += s
    if out.size < entryOf.size then Left(lists.view.mapValues(_.map(_.copy(systems = None))).toMap)
    else
      Right(out.toList.map { k =>
        val on = presence(k).sorted
        entryOf(k).copy(systems = Option.when(on.size < systems.size)(on))
      })

  private def distinct(list: List[Entry]): List[Entry] =
    val seen = mutable.Set.empty[String]
    list.filter(e => seen.add(key(e)))
