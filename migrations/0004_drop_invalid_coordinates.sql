DELETE FROM vehicle_positions
WHERE abs(latitude_e6) > 90000000 OR abs(longitude_e6) > 180000000;
