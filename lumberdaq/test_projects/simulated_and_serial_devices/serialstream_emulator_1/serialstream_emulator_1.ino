
float last_pressure = 2.0;
int index_count = 0;
int count_to_debug_message = 0;

void setup() {
  Serial.begin(115200);  
}


void loop() {
  // generate flowrate
  float pressure = last_pressure + random(-1, 2);

  // Serial.print(index_count); Serial.print(",");
  // Serial.print(flowrate); Serial.print(",");
  Serial.print("#1, ");
  Serial.print(pressure);
  Serial.println(", 0, 1, 1, STBY, 0, 1, 0$");

  if (count_to_debug_message > 50) {
    Serial.println("This is a test debug message.");
    count_to_debug_message = 0;
  }

  index_count += 1;
  count_to_debug_message +=1;
  delay(100);
}
