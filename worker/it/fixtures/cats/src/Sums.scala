package sums

import cats.syntax.all.*

object Sums:
  def total(xs: List[Int]): Int = xs.combineAll
  def pair: Option[(Int, String)] = (Option(1), Option("one")).tupled
