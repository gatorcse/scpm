package shapes

sealed trait Shape
final case class Circle(radius: Double) extends Shape
final case class Square(side: Double) extends Shape

object Shape {
  def area(shape: Shape): Double = shape match {
    case Circle(r) => math.Pi * r * r
  }

  def describe(shape: Shape): String = Units.format(area(shape))
}
