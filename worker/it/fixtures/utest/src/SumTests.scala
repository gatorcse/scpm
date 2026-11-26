package sums

import utest.*

object SumTests extends TestSuite:
  val tests = Tests {
    test("adds") {
      assert(List(1, 2, 3).sum == 6)
    }
  }
